use std::io;

pub(crate) const MAX_AUTH_ERROR_BODY_BYTES: usize = 64 * 1024;
pub(crate) const MAX_AUTH_SUCCESS_BODY_BYTES: usize = 1024 * 1024;

/// Reads a response body without allowing an auth or usage endpoint to allocate
/// unbounded memory. The caller decides whether an over-limit error is fatal or
/// whether an error body can be treated as absent.
pub(crate) async fn read_response_text_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> io::Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(io::Error::other(format!(
            "HTTP response body exceeds {max_bytes} bytes"
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if chunk.len() > max_bytes.saturating_sub(body.len()) {
            return Err(io::Error::other(format!(
                "HTTP response body exceeds {max_bytes} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}
