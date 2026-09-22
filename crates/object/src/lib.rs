//! The Object Service: Bucket/Object domain services that orchestrate `loony-metadata` +
//! `loony-placement` + `loony-erasure` + `loony-storage`. `loony-api` calls into this and contains
//! no business logic of its own (architecture.md §1). Multipart/versioning services
//! land in later phases (PROMPT.md schedules them separately from standalone CRUD).

mod bucket_service;
mod error;
mod object_service;
mod stripe_reader;

pub use bucket_service::BucketService;
pub use error::S3Error;
pub use object_service::{ListObjectsParams, ObjectService};
