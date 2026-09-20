use crate::types::error::Result;
use crate::types::{CompileContext, CompiledPage, RawEntity};
use async_trait::async_trait;

/// 编译器（LLM 编译 → 带评分与 QUG 边的页面）。
#[async_trait]
pub trait Compiler: Send + Sync {
    async fn compile(&self, raw: RawEntity, ctx: &CompileContext) -> Result<CompiledPage>;
}
