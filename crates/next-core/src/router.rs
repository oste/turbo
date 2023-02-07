use anyhow::{bail, Result};
use serde::Deserialize;
use turbo_tasks::{
    primitives::{JsonValueVc, StringsVc},
    Value,
};
use turbo_tasks_fs::{json::parse_json_rope_with_source_context, to_sys_path, FileSystemPathVc};
use turbopack::evaluate_context::node_evaluate_asset_context;
use turbopack_core::{
    asset::AssetVc,
    context::{AssetContext, AssetContextVc},
    resolve::{find_context_file, FindContextFileResult},
    source_asset::SourceAssetVc,
};
use turbopack_dev_server::source::{headers::Headers, query::Query};
use turbopack_ecmascript::{
    chunk::EcmascriptChunkPlaceablesVc, EcmascriptInputTransform, EcmascriptInputTransformsVc,
    EcmascriptModuleAssetType, EcmascriptModuleAssetVc,
};
use turbopack_node::{
    evaluate::{evaluate, JavaScriptValue},
    execution_context::{ExecutionContext, ExecutionContextVc},
    StructuredError,
};

use crate::{
    embed_js::{next_asset, wrap_with_next_js_fs},
    next_config::NextConfigVc,
    next_import_map::get_next_build_import_map,
};

#[turbo_tasks::function]
fn next_configs() -> StringsVc {
    StringsVc::cell(
        ["next.config.mjs", "next.config.js"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
    )
}

#[turbo_tasks::function]
async fn middleware_files(page_extensions: StringsVc) -> Result<StringsVc> {
    let extensions = page_extensions.await?;
    let files = ["middleware.", "src/middleware."]
        .into_iter()
        .flat_map(|f| extensions.iter().map(move |ext| String::from(f) + ext))
        .collect();
    Ok(StringsVc::cell(files))
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct RouterRequest {
    pub method: String,
    pub pathname: String,
    pub query: Query,
    pub headers: Headers,
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct RewriteResponse {
    pub url: String,
    pub headers: Vec<String>,
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct MiddlewareHeadersResponse {
    pub status_code: u16,
    pub headers: Vec<String>,
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
pub struct MiddlewareBodyResponse(pub Vec<u8>);

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
pub struct FullMiddlewareResponse {
    pub headers: MiddlewareHeadersResponse,
    pub body: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum RouterIncomingMessage {
    Rewrite {
        data: RewriteResponse,
    },
    // TODO: Implement
    #[allow(dead_code)]
    MiddlewareHeaders {
        data: MiddlewareHeadersResponse,
    },
    // TODO: Implement
    #[allow(dead_code)]
    MiddlewareBody {
        data: MiddlewareBodyResponse,
    },
    FullMiddleware {
        data: FullMiddlewareResponse,
    },
    Error(StructuredError),
}

#[derive(Debug)]
#[turbo_tasks::value]
pub enum RouterResult {
    Rewrite(RewriteResponse),
    FullMiddleware(FullMiddlewareResponse),
    Error,
}

impl From<RouterIncomingMessage> for RouterResult {
    fn from(value: RouterIncomingMessage) -> Self {
        match value {
            RouterIncomingMessage::Rewrite { data } => Self::Rewrite(data),
            RouterIncomingMessage::FullMiddleware { data } => Self::FullMiddleware(data),
            _ => Self::Error,
        }
    }
}

#[turbo_tasks::function]
async fn extra_config(
    context: AssetContextVc,
    project_path: FileSystemPathVc,
    configs: StringsVc,
) -> Result<EcmascriptChunkPlaceablesVc> {
    let find_config_result = find_context_file(project_path, configs);
    let config_asset = match &*find_config_result.await? {
        FindContextFileResult::Found(config_path, _) => Some(SourceAssetVc::new(*config_path)),
        FindContextFileResult::NotFound(_) => None,
    };
    let Some(config_asset) = config_asset else {
        return Ok(EcmascriptChunkPlaceablesVc::empty());
    };

    let config_chunk = EcmascriptModuleAssetVc::new(
        config_asset.into(),
        context,
        Value::new(EcmascriptModuleAssetType::Typescript),
        EcmascriptInputTransformsVc::cell(vec![EcmascriptInputTransform::TypeScript]),
        context.environment(),
    )
    .as_ecmascript_chunk_placeable();
    Ok(EcmascriptChunkPlaceablesVc::cell(vec![config_chunk]))
}

#[turbo_tasks::function]
async fn extra_configs(
    context: AssetContextVc,
    project_path: FileSystemPathVc,
    page_extensions: StringsVc,
) -> Result<EcmascriptChunkPlaceablesVc> {
    let next_config = extra_config(context, project_path, next_configs()).await?;
    let middleware_config =
        extra_config(context, project_path, middleware_files(page_extensions)).await?;

    let mut concat = next_config.clone_value();
    concat.extend(&*middleware_config);
    Ok(EcmascriptChunkPlaceablesVc::cell(concat))
}

#[turbo_tasks::function]
fn route_executor(context: AssetContextVc, project_path: FileSystemPathVc) -> AssetVc {
    EcmascriptModuleAssetVc::new(
        next_asset(project_path.join("router.js"), "entry/router.ts"),
        context,
        Value::new(EcmascriptModuleAssetType::Typescript),
        EcmascriptInputTransformsVc::cell(vec![EcmascriptInputTransform::TypeScript]),
        context.environment(),
    )
    .into()
}

#[turbo_tasks::function]
pub async fn route(
    execution_context: ExecutionContextVc,
    request: RouterRequestVc,
    next_config: NextConfigVc,
) -> Result<RouterResultVc> {
    let ExecutionContext {
        project_root,
        intermediate_output_path,
    } = *execution_context.await?;
    let project_path = wrap_with_next_js_fs(project_root);
    let context = node_evaluate_asset_context(Some(get_next_build_import_map(project_path)));
    let router_asset = route_executor(context, project_path);
    // TODO this is a hack to get these files watched.
    let extra_configs = extra_configs(context, project_path, next_config.page_extensions());

    let request = serde_json::value::to_value(&*request.await?)?;
    let Some(dir) = to_sys_path(project_root).await? else {
        bail!("Next.js requires a disk path to check for valid routes");
    };
    let result = evaluate(
        project_path,
        router_asset,
        project_root,
        project_root,
        context,
        intermediate_output_path.join("router"),
        Some(extra_configs),
        vec![
            JsonValueVc::cell(request),
            JsonValueVc::cell(dir.to_string_lossy().into()),
        ],
        false,
    )
    .await?;

    match &*result {
        JavaScriptValue::Value(val) => {
            let result: RouterIncomingMessage = parse_json_rope_with_source_context(val)?;
            Ok(RouterResult::from(result).cell())
        }
        JavaScriptValue::Error => Ok(RouterResult::Error.cell()),
        JavaScriptValue::Stream(_) => {
            unimplemented!("Stream not supported now");
        }
    }
}
