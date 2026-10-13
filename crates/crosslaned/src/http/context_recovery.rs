//! Recovery guidance for locally measured, indivisible context overflow.

use crate::stats::ContextOverflowDiagnostics;

pub(super) const LEGACY_INPUT_OVERFLOW_HINT: &str = "Reduce or split the current input, paginate tool results, or shorten required system/tool content. Compacting older history cannot make this input fit.";

pub(super) struct InputOverflowRecovery {
    pub detail: String,
    pub hint: &'static str,
}

pub(super) fn input_overflow_recovery(
    context: &ContextOverflowDiagnostics,
) -> Option<InputOverflowRecovery> {
    let maximum = context.maximum_input_tokens;
    if context.tokens.minimum_input_tokens <= maximum {
        // Retain the native prompt-too-long wording for compactable history so
        // Claude Code can still initiate its automatic recovery.
        return None;
    }
    let (subject, tokens, hint) = if context.tokens.current_message_tokens > maximum {
        (
            "Current input",
            context.tokens.current_message_tokens,
            "Recovery: 1) Stop retrying the same oversized input; split pasted text or attachments, or paginate tool results. 2) Keep this Claude Code session and try /compact, or send a short follow-up without the large input to allow automatic compaction. 3) If compact or the follow-up still fails, use Esc Esc or /rewind in interactive Claude Code to edit only the oversized turn, then resend smaller parts. Compacting older history alone cannot make the current input fit.",
        )
    } else {
        (
            "Required input",
            context.tokens.minimum_input_tokens,
            "Recovery: reduce required system instructions, enabled tool definitions, attachments, or current tool results; split or paginate large content. Keep the same session and retry with the reduced input. Run /compact only if older history still needs compression; compacting history cannot shrink this required input.",
        )
    };
    // Claude Code replaces errors containing "prompt is too long" with a fixed
    // message, dropping all recovery instructions. Only locally diagnosed input
    // that cannot fit after history compaction uses this alternative prose.
    // The HTTP status, context_length_exceeded code and numeric diagnostics are
    // retained. Generate the detail from counts, never from request content.
    Some(InputOverflowRecovery {
        detail: format!(
            "{subject} exceeds this model's input budget: {tokens} tokens (limit {maximum})."
        ),
        hint,
    })
}
