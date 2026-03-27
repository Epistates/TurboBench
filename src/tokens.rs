/// Approximate bytes-per-token ratio for JSON/English text with Claude models.
///
/// Rough heuristic (~4 bytes per token). For exact counts, use
/// Anthropic's `/v1/messages/count_tokens` API.
const BYTES_PER_TOKEN: f64 = 4.0;

/// Estimate token count from byte length.
pub fn estimate_tokens(bytes: usize) -> usize {
    (bytes as f64 / BYTES_PER_TOKEN).ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(100), 25);
        assert_eq!(estimate_tokens(3), 1);
    }
}
