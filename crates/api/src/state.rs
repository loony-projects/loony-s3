use std::sync::Arc;

use s3_auth::CredentialProvider;
use s3_object::{BucketService, ObjectService};

#[derive(Clone)]
pub struct AppState {
    pub buckets: Arc<BucketService>,
    pub objects: Arc<ObjectService>,
    pub credentials: Arc<dyn CredentialProvider>,
    /// SigV4 region scope this server verifies requests against (prompt §52 credential
    /// scope `{date}/{region}/{service}/aws4_request`).
    pub region: String,
}
