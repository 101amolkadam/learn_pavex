use pavex_runtime::{extract::body::JsonBody, http::StatusCode};
use secrecy::Secret;

#[derive(serde::Deserialize)]
pub struct UpdateUser {
    pub user: UpdatedDetails,
}

#[derive(serde::Deserialize)]
pub struct UpdatedDetails {
    pub email: Option<String>,
    pub username: Option<String>,
    pub password: Option<Secret<String>>,
    pub bio: Option<String>,
    pub image: Option<String>,
}

pub fn update_user(_body: JsonBody<UpdateUser>) -> StatusCode {
    StatusCode::OK
}