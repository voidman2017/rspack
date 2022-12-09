use anyhow::anyhow;
use async_trait::async_trait;
use dashmap::DashMap;
use hashbrown::hash_map::DefaultHashBuilder;
use rayon::prelude::*;
use rspack_core::rspack_sources::{
  BoxSource, CachedSource, ConcatSource, MapOptions, RawSource, Source, SourceExt, SourceMap,
  SourceMapSource, SourceMapSourceOptions,
};
use rspack_core::{
  get_js_chunk_filename_template, runtime_globals, AstOrSource, ChunkKind, ChunkUkey, Compilation,
  FilenameRenderOptions, GenerateContext, GenerationResult, Module, ModuleAst, ModuleType,
  ParseContext, ParseResult, ParserAndGenerator, PathData, Plugin, PluginContext,
  PluginProcessAssetsOutput, PluginRenderManifestHookOutput, ProcessAssetsArgs,
  RenderManifestEntry, SourceType, TargetPlatform,
};
use rspack_error::{Error, IntoTWithDiagnosticArray, Result, TWithDiagnosticArray};
use swc_core::base::try_with_handler;
use swc_core::base::{config::JsMinifyOptions, BoolOrDataConfig};
use swc_core::common::util::take::Take;
use swc_core::common::FileName;
use swc_core::common::GLOBALS;
use swc_core::ecma::ast;
use swc_core::ecma::minifier::option::terser::TerserCompressorOptions;
use tracing::instrument;

use crate::utils::{
  get_swc_compiler, syntax_by_module_type, wrap_eval_source_map, wrap_module_function,
};
use crate::visitors::{run_after_pass, run_before_pass, DependencyScanner};

#[derive(Debug)]
pub struct JsPlugin {
  eval_source_map_cache: DashMap<Box<dyn Source>, Box<dyn Source>, DefaultHashBuilder>,
}

impl JsPlugin {
  pub fn new() -> Self {
    Self {
      eval_source_map_cache: Default::default(),
    }
  }

  pub fn generate_chunk_entry_code(
    &self,
    compilation: &Compilation,
    chunk_ukey: &ChunkUkey,
  ) -> BoxSource {
    let entry_modules_uri = compilation.chunk_graph.get_chunk_entry_modules(chunk_ukey);
    let entry_modules_id = entry_modules_uri
      .into_iter()
      .filter_map(|entry_module_identifier| {
        compilation
          .module_graph
          .module_graph_module_by_identifier(entry_module_identifier)
          .map(|module| module.id(&compilation.chunk_graph))
      })
      .collect::<Vec<_>>();
    // let namespace = &compilation.options.output.unique_name;
    let sources = entry_modules_id
      .iter()
      .map(|id| {
        if let Some(library) = &compilation.options.output.library && !library.is_empty() {
          RawSource::from(format!(r#"{} = {}("{}");"#, library, runtime_globals::REQUIRE, id))
        } else {
          RawSource::from(format!(r#"{}("{}");"#, runtime_globals::REQUIRE, id))
        }
      })
      .collect::<Vec<_>>();
    let concat = ConcatSource::new(sources);
    concat.boxed()
  }

  pub fn render_require(&self, args: &rspack_core::RenderManifestArgs) -> BoxSource {
    let runtime_requirements = args
      .compilation
      .chunk_graph
      .get_chunk_runtime_requirements(&args.chunk_ukey);

    let mut sources = ConcatSource::default();

    sources.add(RawSource::from(
      r#"// Check if module is in cache
        var cachedModule = __webpack_module_cache__[moduleId];
        if (cachedModule !== undefined) {
          return cachedModule.exports;
        }
        // Create a new module (and put it into the cache)
        var module = (__webpack_module_cache__[moduleId] = {
          // no module.id needed
          // no module.loaded needed
          exports: {}
        });
        // Execute the module function
      "#,
    ));

    if runtime_requirements.contains(runtime_globals::INTERCEPT_MODULE_EXECUTION) {
      sources.add(RawSource::from(
        r#"var execOptions = { id: moduleId, module: module, factory: __webpack_modules__[moduleId], require: __webpack_require__ };
        __webpack_require__.i.forEach(function(handler) { handler(execOptions); });
        module = execOptions.module;
        execOptions.factory.call(module.exports, module, module.exports, execOptions.require);"#,
      ));
    } else {
      sources.add(RawSource::from(
        "__webpack_modules__[moduleId](module, module.exports, __webpack_require__);\n",
      ));
    }

    sources.add(RawSource::from(
      "// Return the exports of the module\n return module.exports;\n",
    ));

    sources.boxed()
  }

  pub fn render_bootstrap(&self, args: &rspack_core::RenderManifestArgs) -> BoxSource {
    let runtime_requirements = args
      .compilation
      .chunk_graph
      .get_chunk_runtime_requirements(&args.chunk_ukey);

    let module_factories = runtime_requirements.contains(runtime_globals::MODULE_FACTORIES);

    let mut sources = ConcatSource::default();

    sources.add(RawSource::from(
      "// The module cache\n var __webpack_module_cache__ = {};\n",
    ));
    sources.add(RawSource::from(
      "function __webpack_require__(moduleId) {\n",
    ));
    sources.add(self.render_require(args));
    sources.add(RawSource::from("\n}\n"));

    if module_factories || runtime_requirements.contains(runtime_globals::MODULE_FACTORIES_ADD_ONLY)
    {
      sources.add(RawSource::from(
        "// expose the modules object (__webpack_modules__)\n __webpack_require__.m = __webpack_modules__;\n",
      ));
    }

    if runtime_requirements.contains(runtime_globals::MODULE_CACHE) {
      sources.add(RawSource::from(
        "// expose the module cache\n __webpack_require__.c = __webpack_module_cache__;\n",
      ));
    }

    if runtime_requirements.contains(runtime_globals::INTERCEPT_MODULE_EXECUTION) {
      sources.add(RawSource::from(
        "// expose the module execution interceptor\n __webpack_require__.i = [];\n",
      ));
    }

    sources.boxed()
  }

  pub fn render_main(&self, args: &rspack_core::RenderManifestArgs) -> Result<BoxSource> {
    let compilation = args.compilation;
    let chunk = args.chunk();
    let mut sources = ConcatSource::default();
    sources.add(RawSource::from("var __webpack_modules__ = "));
    sources.add(self.render_chunk_modules(args)?);
    sources.add(RawSource::from("\n"));
    sources.add(self.render_bootstrap(args));
    sources.add(self.render_runtime_modules(args)?);
    if chunk.has_entry_module(&args.compilation.chunk_graph) {
      // TODO: how do we handle multiple entry modules?
      sources.add(self.generate_chunk_entry_code(compilation, &args.chunk_ukey));
    }
    Ok(self.render_iife(sources.boxed(), args))
  }

  pub fn render_chunk(&self, args: &rspack_core::RenderManifestArgs) -> Result<BoxSource> {
    match args.compilation.options.target.platform {
      TargetPlatform::Node(_) => self.render_node_chunk(args),
      _ => self.render_web_chunk(args),
    }
  }

  pub fn render_node_chunk(&self, args: &rspack_core::RenderManifestArgs) -> Result<BoxSource> {
    let chunk = args.chunk();
    let mut sources = ConcatSource::default();
    sources.add(RawSource::from(format!(
      r#"exports.ids = ["{}"];
      exports.modules = "#,
      &chunk.id.to_owned()
    )));
    sources.add(self.render_chunk_modules(args)?);
    sources.add(RawSource::from(";"));
    if chunk.has_entry_module(&args.compilation.chunk_graph) {
      let entry_point = {
        let entry_points = args
          .compilation
          .chunk_graph
          .get_chunk_entry_modules_with_chunk_group(&chunk.ukey);

        let entry_point_ukey = entry_points
          .iter()
          .next()
          .ok_or_else(|| anyhow!("should has entry point ukey"))?;

        args
          .compilation
          .chunk_group_by_ukey
          .get(entry_point_ukey)
          .ok_or_else(|| anyhow!("should has entry point"))?
      };

      let runtime_chunk_filename = {
        let runtime_chunk = args
          .compilation
          .chunk_by_ukey
          .get(&entry_point.get_runtime_chunk())
          .ok_or_else(|| anyhow!("should has runtime chunk"))?;

        let hash = Some(runtime_chunk.get_render_hash());
        args
          .compilation
          .options
          .output
          .chunk_filename
          .render(FilenameRenderOptions {
            filename: runtime_chunk.name.clone(),
            extension: Some(".js".to_string()),
            id: Some(runtime_chunk.id.clone()),
            contenthash: hash.clone(),
            chunkhash: hash.clone(),
            hash,
            ..Default::default()
          })
      };

      sources.add(RawSource::from(format!(
        "\nvar {} = require('./{}')",
        runtime_globals::REQUIRE,
        runtime_chunk_filename
      )));
      sources.add(RawSource::from(format!(
        "\n{}(exports)\n",
        runtime_globals::EXTERNAL_INSTALL_CHUNK,
      )));
      sources.add(self.generate_chunk_entry_code(args.compilation, &args.chunk_ukey));
    }
    Ok(sources.boxed())
  }

  pub fn render_web_chunk(&self, args: &rspack_core::RenderManifestArgs) -> Result<BoxSource> {
    let chunk = args.chunk();
    let mut sources = ConcatSource::default();
    sources.add(RawSource::from(format!(
      r#"(self['webpackChunkwebpack'] = self['webpackChunkwebpack'] || []).push([["{}"], "#,
      &args.chunk().id.to_owned(),
    )));
    sources.add(self.render_chunk_modules(args)?);
    if chunk.has_entry_module(&args.compilation.chunk_graph) {
      sources.add(RawSource::from(","));
      sources.add(RawSource::from(format!(
        "function({}) {{\n",
        runtime_globals::REQUIRE
      )));
      sources.add(self.generate_chunk_entry_code(args.compilation, &args.chunk_ukey));
      sources.add(RawSource::from("\n}\n"));
    }
    sources.add(RawSource::from("]);"));
    Ok(sources.boxed())
  }

  pub fn render_iife(
    &self,
    content: BoxSource,
    args: &rspack_core::RenderManifestArgs,
  ) -> BoxSource {
    let mut sources = ConcatSource::default();
    if let Some(library) = &args.compilation.options.output.library && !library.is_empty() {
      sources.add(RawSource::from(format!("var {};\n", library)));
    }
    sources.add(RawSource::from("(function() {\n"));
    sources.add(content);
    sources.add(RawSource::from("\n})();\n"));
    sources.boxed()
  }

  pub fn render_chunk_runtime_modules(
    &self,
    args: &rspack_core::RenderManifestArgs,
  ) -> Result<BoxSource> {
    let runtime_modules_sources = self.render_runtime_modules(args)?;
    if runtime_modules_sources.source().is_empty() {
      return Ok(runtime_modules_sources);
    }

    let mut sources = ConcatSource::default();
    sources.add(RawSource::from(format!(
      "function({}) {{\n",
      runtime_globals::REQUIRE
    )));
    sources.add(runtime_modules_sources);
    sources.add(RawSource::from("\n}\n"));
    Ok(sources.boxed())
  }

  pub fn render_runtime_modules(
    &self,
    args: &rspack_core::RenderManifestArgs,
  ) -> Result<BoxSource> {
    let mut sources = ConcatSource::default();
    args
      .compilation
      .chunk_graph
      .get_chunk_runtime_modules_in_order(&args.chunk_ukey)
      .iter()
      .filter_map(|identifier| args.compilation.runtime_modules.get(identifier))
      .for_each(|module| {
        sources.add(RawSource::from(format!("// {}\n", module.identifier())));
        sources.add(RawSource::from("(function() {\n"));
        sources.add(module.generate(args.compilation));
        sources.add(RawSource::from("\n})();\n"));
      });
    Ok(sources.boxed())
  }

  pub fn render_chunk_modules(&self, args: &rspack_core::RenderManifestArgs) -> Result<BoxSource> {
    let compilation = args.compilation;
    let module_graph = &compilation.module_graph;
    let mut ordered_modules = compilation.chunk_graph.get_chunk_modules_by_source_type(
      &args.chunk_ukey,
      SourceType::JavaScript,
      module_graph,
    );
    let chunk = args.chunk();

    ordered_modules.sort_by_key(|m| &m.module_identifier);

    let module_code_array = ordered_modules
      .par_iter()
      .filter(|mgm| mgm.used)
      .map(|mgm| {
        let code_gen_result = compilation
          .code_generation_results
          .get(&mgm.module_identifier, Some(&chunk.runtime))?;

        code_gen_result
          .get(&SourceType::JavaScript)
          .map(|result| {
            let mut module_source = result.ast_or_source.clone().try_into_source()?;

            if args.compilation.options.devtool.eval()
              && args.compilation.options.devtool.source_map()
            {
              module_source =
                wrap_eval_source_map(module_source, &self.eval_source_map_cache, args.compilation)?;
            }

            if mgm.module_type.is_css_like() && compilation.options.dev_server.hot {
              // inject css hmr runtime
              module_source = ConcatSource::new([
                module_source,
                RawSource::from(
                  r#"
if (module.hot) {
  module.hot.accept();
}
"#,
                )
                .boxed(),
              ])
              .boxed();
              Ok(wrap_module_function(
                module_source,
                mgm.id(&compilation.chunk_graph),
              ))
            } else {
              Ok(wrap_module_function(
                module_source,
                mgm.id(&compilation.chunk_graph),
              ))
            }
          })
          .transpose()
      })
      .collect::<Result<Vec<Option<BoxSource>>>>()?;

    let module_sources = module_code_array
      .into_par_iter()
      .flatten()
      .fold(ConcatSource::default, |mut output, cur| {
        output.add(cur);
        output
      })
      .collect::<Vec<ConcatSource>>();

    let mut sources = ConcatSource::default();
    sources.add(RawSource::from("{\n"));
    sources.add(CachedSource::new(ConcatSource::new(module_sources)));
    sources.add(RawSource::from("\n}"));

    Ok(CachedSource::new(sources).boxed())
  }
}

impl Default for JsPlugin {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Debug)]
pub struct JavaScriptParserAndGenerator {}

impl JavaScriptParserAndGenerator {
  fn new() -> Self {
    Self {}
  }
}

static SOURCE_TYPES: &[SourceType; 1] = &[SourceType::JavaScript];

impl ParserAndGenerator for JavaScriptParserAndGenerator {
  fn source_types(&self) -> &[SourceType] {
    SOURCE_TYPES
  }

  fn size(&self, module: &dyn Module, _source_type: &SourceType) -> f64 {
    module.original_source().map_or(0, |source| source.size()) as f64
  }

  #[instrument(name = "js:parse", skip_all)]
  fn parse(&mut self, parse_context: ParseContext) -> Result<TWithDiagnosticArray<ParseResult>> {
    let ParseContext {
      source,
      module_type,
      resource_data,
      compiler_options,
      ..
    } = parse_context;

    if !module_type.is_js_like() {
      return Err(Error::InternalError(format!(
        "`module_type` {:?} not supported for `JsParser`",
        module_type
      )));
    }

    let syntax = syntax_by_module_type(
      &resource_data.resource_path,
      module_type,
      compiler_options.builtins.decorator.is_some(),
    );
    let (mut ast, diagnostics) = match crate::ast::parse(
      source.source().to_string(),
      syntax,
      &resource_data.resource_path,
      module_type,
    ) {
      Ok(ast) => (ast, Vec::new()),
      Err(diagnostics) => (
        rspack_core::ast::javascript::Ast::new(ast::Program::Module(ast::Module::dummy())),
        diagnostics.into(),
      ),
    };

    run_before_pass(resource_data, &mut ast, compiler_options, syntax)?;

    let dep_scanner = ast.visit(|program, context| {
      let mut dep_scanner = DependencyScanner::new(context.unresolved_mark);
      program.visit_with(&mut dep_scanner);
      dep_scanner
    });

    Ok(
      ParseResult {
        ast_or_source: AstOrSource::Ast(ModuleAst::JavaScript(ast)),
        dependencies: dep_scanner.dependencies.into_iter().collect(),
      }
      .with_diagnostic(diagnostics),
    )
  }

  #[allow(clippy::unwrap_in_result)]
  #[instrument(name = "js:generate", skip_all)]
  fn generate(
    &self,
    ast_or_source: &AstOrSource,
    module: &dyn Module,
    generate_context: &mut GenerateContext,
  ) -> Result<GenerationResult> {
    if matches!(
      generate_context.requested_source_type,
      SourceType::JavaScript
    ) {
      // TODO: this should only return AST for javascript only, It's a fast pass, defer to another pr to solve this.
      // Ok(ast_or_source.to_owned().into())
      let mut ast = ast_or_source
        .to_owned()
        .try_into_ast()?
        .try_into_javascript()?;
      run_after_pass(&mut ast, module, generate_context);
      let output = crate::ast::stringify(&ast, &generate_context.compilation.options.devtool)?;
      if let Some(map) = output.map {
        Ok(GenerationResult {
          ast_or_source: SourceMapSource::new(SourceMapSourceOptions {
            value: output.code,
            source_map: SourceMap::from_json(&map)
              .map_err(|e| rspack_error::Error::InternalError(e.to_string()))?,
            name: module.try_as_normal_module()?.request().to_string(),
            original_source: {
              Some(
                // Safety: you can sure that `build` is called before code generation, so that the `original_source` is exist
                module
                  .original_source()
                  .expect("Failed to get original source, please file an issue.")
                  .source()
                  .to_string(),
              )
            },
            inner_source_map: {
              // Safety: you can sure that `build` is called before code generation, so that the `original_source` is exist
              module
                .original_source()
                .expect("Failed to get original source, please file an issue.")
                .map(&MapOptions::default())
            },
            remove_original_source: false,
          })
          .boxed()
          .into(),
        })
      } else {
        Ok(GenerationResult {
          ast_or_source: RawSource::from(output.code).boxed().into(),
        })
      }
    } else {
      Err(Error::InternalError(format!(
        "Unsupported source type {:?} for plugin JavaScript",
        generate_context.requested_source_type,
      )))
    }
  }
}

#[async_trait]
impl Plugin for JsPlugin {
  fn name(&self) -> &'static str {
    "javascript"
  }
  fn apply(&mut self, ctx: PluginContext<&mut rspack_core::ApplyContext>) -> Result<()> {
    let create_parser_and_generator =
      move || Box::new(JavaScriptParserAndGenerator::new()) as Box<dyn ParserAndGenerator>;

    ctx
      .context
      .register_parser_and_generator_builder(ModuleType::Js, Box::new(create_parser_and_generator));
    ctx
      .context
      .register_parser_and_generator_builder(ModuleType::Ts, Box::new(create_parser_and_generator));
    ctx.context.register_parser_and_generator_builder(
      ModuleType::Tsx,
      Box::new(create_parser_and_generator),
    );
    ctx.context.register_parser_and_generator_builder(
      ModuleType::Jsx,
      Box::new(create_parser_and_generator),
    );

    Ok(())
  }

  fn render_manifest(
    &self,
    _ctx: PluginContext,
    args: rspack_core::RenderManifestArgs,
  ) -> PluginRenderManifestHookOutput {
    let compilation = args.compilation;
    let chunk = args.chunk();
    let filename = args.chunk().id.to_owned();

    let is_hot_update_chunk = matches!(chunk.kind, ChunkKind::HotUpdate);
    let source = if is_hot_update_chunk {
      let mut source = ConcatSource::default();
      source.add(RawSource::Source(format!(
        "self['hotUpdate']('{}', ",
        filename
      )));
      source.add(self.render_chunk_modules(&args)?);
      source.add(RawSource::Source(",".to_string()));
      source.add(self.render_chunk_runtime_modules(&args)?);
      source.add(RawSource::Source(");".to_string()));
      source.boxed()
    } else if chunk.has_runtime(&compilation.chunk_group_by_ukey) {
      self.render_main(&args)?
    } else {
      self.render_chunk(&args)?
    };
    // let hash = Some(get_hash(compilation).to_string());
    // let hash = None;
    // let chunkhash = Some(get_chunkhash(compilation, &args.chunk_ukey, module_graph).to_string());
    // let chunkhash = None;
    // let contenthash = Some(chunk.hash.clone());
    let filename_template = get_js_chunk_filename_template(
      chunk,
      &compilation.options.output,
      &compilation.chunk_group_by_ukey,
    );
    let hash = Some(chunk.get_render_hash());

    let output_path = filename_template.render(FilenameRenderOptions {
      filename: chunk.name.clone(),
      extension: Some(".js".to_owned()),
      id: Some(chunk.id.to_string()),
      contenthash: hash.clone(),
      chunkhash: hash.clone(),
      hash,
      ..Default::default()
    });

    let path_options = PathData {
      chunk_ukey: args.chunk_ukey,
    };
    Ok(vec![RenderManifestEntry::new(
      source,
      output_path,
      path_options,
    )])
  }

  async fn process_assets(
    &mut self,
    _ctx: PluginContext,
    args: ProcessAssetsArgs<'_>,
  ) -> PluginProcessAssetsOutput {
    let compilation = args.compilation;
    let minify = compilation.options.builtins.minify;
    if !minify.enable {
      return Ok(());
    }

    let swc_compiler = get_swc_compiler();
    compilation
      .assets
      .par_iter_mut()
      .filter(|(filename, _)| {
        filename.ends_with(".js") || filename.ends_with(".cjs") || filename.ends_with(".mjs")
      })
      .try_for_each(|(filename, original)| -> Result<()> {
        // In theory, if a js source is minimized it has high possibility has been tree-shaked.
        if original.get_info().minimized {
          return Ok(());
        }

        let input = original.get_source().source().to_string();
        let input_source_map = original.get_source().map(&MapOptions::default());
        let output = GLOBALS.set(&Default::default(), || {
          try_with_handler(swc_compiler.cm.clone(), Default::default(), |handler| {
            let fm = swc_compiler
              .cm
              .new_source_file(FileName::Custom(filename.to_string()), input.clone());
            swc_compiler.minify(
              fm,
              handler,
              &JsMinifyOptions {
                compress: BoolOrDataConfig::from_obj(TerserCompressorOptions {
                  passes: minify.passes,
                  ..Default::default()
                }),
                source_map: BoolOrDataConfig::from_bool(input_source_map.is_some()),
                inline_sources_content: false, // don't need this since we have inner_source_map in SourceMapSource
                emit_source_map_columns: !compilation.options.devtool.cheap(),
                ..Default::default()
              },
            )
          })
        })?;
        let source = if let Some(map) = &output.map {
          SourceMapSource::new(SourceMapSourceOptions {
            value: output.code,
            name: format!("<{filename}>"), // match with swc FileName::Custom...
            source_map: SourceMap::from_json(map)
              .map_err(|e| rspack_error::Error::InternalError(e.to_string()))?,
            original_source: Some(input),
            inner_source_map: input_source_map,
            remove_original_source: true,
          })
          .boxed()
        } else {
          RawSource::from(output.code).boxed()
        };
        original.set_source(source);
        original.get_info_mut().minimized = true;
        Ok(())
      })?;

    Ok(())
  }
}
