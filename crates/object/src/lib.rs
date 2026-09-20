//! The Object Service: Bucket/Object domain services that orchestrate `s3-metadata` +
//! `s3-placement` + `s3-erasure` + `s3-storage`. `s3-api` calls into this and contains
//! no business logic of its own (architecture.md §1). Multipart/versioning services
//! land in later phases (PROMPT.md schedules them separately from standalone CRUD).

mod bucket_service;
mod error;
mod object_service;
mod stripe_reader;

pub use bucket_service::BucketService;
pub use error::S3Error;
pub use object_service::{ListObjectsParams, ObjectService};
