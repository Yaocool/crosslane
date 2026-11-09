use crate::{KiroAssistantMessage, KiroUserInputMessage, LocalSources};
use sha2::{Digest, Sha256};

struct SourceDigest(Sha256);

impl std::io::Write for SourceDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SourceDigest {
    fn field(&mut self, text: &str) {
        // Length framing preserves field boundaries without JSON-escaping
        // large text and base64 fields on each provenance validation.
        self.0.update((text.len() as u64).to_le_bytes());
        self.0.update(text.as_bytes());
    }

    fn json(&mut self, value: impl serde::Serialize) {
        serde_json::to_writer(self, &value).expect("Kiro source fields serialize");
    }
}

impl LocalSources {
    pub(crate) fn user_digest(message: &KiroUserInputMessage) -> [u8; 32] {
        Self::user_digest_with_media(message, true)
    }

    fn user_digest_with_media(message: &KiroUserInputMessage, include_media: bool) -> [u8; 32] {
        let mut digest = SourceDigest(Sha256::new());
        digest.field(&message.content);
        digest.0.update((message.images.len() as u64).to_le_bytes());
        for image in &message.images {
            digest.field(&image.format);
            if include_media {
                digest.field(&image.source.bytes);
            }
        }
        digest
            .0
            .update((message.documents.len() as u64).to_le_bytes());
        for document in &message.documents {
            digest.field(&document.format);
            digest.field(&document.name);
            if include_media {
                digest.field(&document.source.bytes);
            }
            digest.json(&document.citations);
        }
        digest.json(
            message
                .user_input_message_context
                .as_ref()
                .map(|c| c.tool_results.as_slice())
                .unwrap_or_default(),
        );
        digest.0.finalize().into()
    }

    pub(crate) fn record_user(&mut self, message: &KiroUserInputMessage) {
        self.translated_digest = Self::user_digest_with_media(message, false);
        self.media_sources = user_media(message).map(std::sync::Arc::from).collect();
    }
    pub(crate) fn assistant_digest(message: &KiroAssistantMessage) -> [u8; 32] {
        let mut digest = SourceDigest(Sha256::new());
        digest.field(&message.content);
        digest.json(&message.tool_uses);
        digest.0.finalize().into()
    }
    pub(crate) fn for_user(message: &KiroUserInputMessage) -> Option<&Self> {
        message.local_sources.as_deref().filter(|sources| {
            sources.translated_digest == Self::user_digest_with_media(message, false)
                && user_media(message).eq(sources.media_sources.iter().map(|s| s.as_ref()))
        })
    }
    pub(crate) fn for_assistant(message: &KiroAssistantMessage) -> Option<&Self> {
        message
            .local_sources
            .as_deref()
            .filter(|sources| sources.translated_digest == Self::assistant_digest(message))
    }
}

fn user_media(message: &KiroUserInputMessage) -> impl Iterator<Item = &str> {
    message
        .images
        .iter()
        .map(|image| image.source.bytes.as_str())
        .chain(
            message
                .documents
                .iter()
                .map(|document| document.source.bytes.as_str()),
        )
}
