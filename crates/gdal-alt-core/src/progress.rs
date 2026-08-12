use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::LazyLock;

static BAR_STYLE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template(
        "{prefix:>12.cyan.bold} {bar:40.cyan/blue} {pos:>4}/{len:<4} {elapsed_precise}",
    )
    .expect("valid progress template")
    .progress_chars("█▓░")
});

pub struct ProgressTracker {
    multi: Option<MultiProgress>,
}

impl ProgressTracker {
    pub fn new(enabled: bool) -> Self {
        Self {
            multi: enabled.then(MultiProgress::new),
        }
    }

    pub fn stage(&self, label: &str, total: u64) -> StageBar {
        match &self.multi {
            Some(multi) => {
                let bar = multi.add(ProgressBar::new(total));
                bar.set_style(BAR_STYLE.clone());
                bar.set_prefix(label.to_owned());
                StageBar::Live(bar)
            }
            None => StageBar::Noop,
        }
    }

    pub fn finish(&self) {
        if let Some(multi) = &self.multi {
            multi.clear().ok();
        }
    }
}

#[derive(Clone)]
pub enum StageBar {
    Live(ProgressBar),
    Noop,
}

impl StageBar {
    pub fn inc(&self, n: u64) {
        if let Self::Live(bar) = self {
            bar.inc(n);
        }
    }

    pub fn done(&self, message: &str) {
        if let Self::Live(bar) = self {
            bar.finish_with_message(message.to_owned());
        }
    }
}
