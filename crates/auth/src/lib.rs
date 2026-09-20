//! SigV4 request verification, presigned URL validation, and the credential lookup
//! trait. Authentication only ("who are you?") — authorization ("may you do this?") is
//! `s3-object`'s concern (architecture.md §55). See docs/architecture.md §24.

mod canonical;
mod credential;
mod sign;
mod verify;

pub use credential::{CredentialProvider, SigningCredential};
pub use sign::{amz_date_now, sign_header_auth, sign_presigned_query};
pub use verify::{AuthError, RequestParts, VerifiedRequest, verify};
