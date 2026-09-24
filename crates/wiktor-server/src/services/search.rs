//! Search 服务（spec step7 §3 D1/D2/D5；B3 填充 QueryEngine 装配）。
//! The Search service (spec step7 §3 D1/D2/D5; B3 fills in the QueryEngine
//! assembly).
//!
//! B1 空壳：trait 实现返回 UNIMPLEMENTED，保证 proto 生成代码可编译注册。
//! B1 shell: the trait impl returns UNIMPLEMENTED, keeping the generated code
//! compilable and registered.

use crate::grpc::v1::search_server::Search;
use crate::grpc::v1::{SearchRequest, SearchResponse};

/// Search gRPC handler（B3 注入 QueryEngine）。
/// The Search gRPC handler (B3 injects the QueryEngine).
#[derive(Debug, Default)]
pub struct SearchService {
    // B3: engine: Arc<QueryEngine<ConfiguredVectorStore>>
}

#[tonic::async_trait]
impl Search for SearchService {
    async fn search(
        &self,
        _request: tonic::Request<SearchRequest>,
    ) -> std::result::Result<tonic::Response<SearchResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Search.Search not yet wired"))
    }
}
