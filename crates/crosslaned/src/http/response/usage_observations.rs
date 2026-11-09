//! Success-path Kiro observations. The raw parser remains the error ledger's
//! authority; these bounded candidates implement the optional zero policy.

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct SuccessUsage {
    pub input: Option<u64>,
    pub uncached: Option<u64>,
    pub total: Option<u64>,
    pub output: Option<u64>,
    pub read: u64,
    pub write: u64,
    pub observed: bool,
    /// Fields seen as explicit zero, for content-free diagnostics.
    pub zero_fields: u8,
    pub present_fields: u8,
    pub positive_fields: u8,
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct NormalizedUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub read: u64,
    pub write: u64,
    pub adjusted: bool,
}

impl SuccessUsage {
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &mut self,
        input: Option<u64>,
        uncached: Option<u64>,
        total: Option<u64>,
        output: Option<u64>,
        read: Option<u64>,
        write: Option<u64>,
    ) {
        self.observed = true;
        for (index, value) in [input, uncached, total, output, read, write]
            .iter()
            .enumerate()
        {
            if value.is_some() {
                self.present_fields |= 1 << index;
            }
            if *value == Some(0) {
                self.zero_fields |= 1 << index;
            }
            if value.is_some_and(|n| n > 0) {
                self.positive_fields |= 1 << index;
            }
        }
        // A complete positive cache observation may explicitly clear one side.
        // Split metadata fills only the fields actually present. An all-zero
        // group is a placeholder and cannot erase a previous positive group.
        let positive_cache = read.is_some_and(|n| n > 0) || write.is_some_and(|n| n > 0);
        if positive_cache {
            if let Some(n) = read {
                self.read = n;
            }
            if let Some(n) = write {
                self.write = n;
            }
        }
        if let Some(n) = input.filter(|n| *n > 0) {
            self.input = Some(n);
        }
        if let Some(n) = total.filter(|n| *n > 0) {
            self.total = Some(n);
        }
        if let Some(n) = output {
            if n > 0 || self.output.is_none() {
                self.output = Some(n);
            }
        }
        if let Some(n) = uncached {
            let placeholder_group = !positive_cache && (read == Some(0) || write == Some(0));
            if n > 0
                || positive_cache
                || (!placeholder_group
                    && (self.uncached.is_none() || self.read > 0 || self.write > 0))
            {
                self.uncached = Some(n);
            }
        }
    }

    pub fn normalized(&self, generated: bool) -> NormalizedUsage {
        let cache = self.read.saturating_add(self.write);
        let input = self
            .uncached
            .filter(|n| *n > 0 || cache > 0)
            .map(|n| n.saturating_add(cache))
            .or(self.input)
            .or_else(|| {
                self.total
                    .zip(self.output)
                    .and_then(|(total, output)| total.checked_sub(output))
            })
            .filter(|n| *n > 0);
        NormalizedUsage {
            input: input.map(|n| n.max(cache)),
            output: self.output.filter(|n| *n > 0 || !generated),
            read: self.read,
            write: self.write,
            adjusted: input.is_some_and(|n| cache > n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positive_corrections_can_decrease_but_zero_placeholder_groups_cannot_erase() {
        let mut usage = SuccessUsage::default();
        usage.observe(Some(100), None, None, Some(20), Some(40), Some(10));
        usage.observe(Some(80), None, None, Some(15), None, None);
        usage.observe(Some(0), Some(0), Some(0), Some(0), Some(0), Some(0));
        let normalized = usage.normalized(true);
        assert_eq!(
            (
                normalized.input,
                normalized.output,
                normalized.read,
                normalized.write
            ),
            (Some(80), Some(15), 40, 10)
        );
        usage.observe(None, Some(0), None, None, Some(50), Some(0));
        let normalized = usage.normalized(true);
        assert_eq!(
            (normalized.input, normalized.read, normalized.write),
            (Some(50), 50, 0)
        );
    }

    #[test]
    fn cache_larger_than_reported_input_is_adjusted_and_split_cache_is_retained() {
        let mut usage = SuccessUsage::default();
        usage.observe(Some(10), None, None, Some(5), Some(20), None);
        usage.observe(None, None, None, None, None, Some(30));
        let normalized = usage.normalized(true);
        assert_eq!(normalized.input, Some(50));
        assert!(normalized.adjusted);
        assert_eq!((normalized.read, normalized.write), (20, 30));
    }
}

/// Bounded source sets, accumulated independently for each usage dimension.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UsageSources {
    pub input: u8,
    pub output: u8,
    pub cache: u8,
}

impl UsageSources {
    pub const UPSTREAM: u8 = 1;
    pub const LOCAL: u8 = 2;
    pub const ADJUSTED: u8 = 4;
    pub const UNREPORTED: u8 = 8;

    pub fn merge(&mut self, other: Self) {
        self.input |= other.input;
        self.output |= other.output;
        self.cache |= other.cache;
    }

    pub fn name(source: u8) -> &'static str {
        match source {
            Self::UPSTREAM => "upstream",
            Self::LOCAL => "local",
            Self::ADJUSTED => "adjusted",
            0 | Self::UNREPORTED => "unreported",
            _ => "mixed",
        }
    }
}
