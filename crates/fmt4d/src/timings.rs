use std::time::{Duration, Instant};

/// Per-file timing breakdown for each formatting phase.
#[derive(Debug, Clone)]
pub struct Timings {
    pub parse: Duration,
    pub comments: Duration,
    pub doc_build: Duration,
    pub render: Duration,
    pub post_process: Duration,
}

impl Timings {
    pub fn total(&self) -> Duration {
        self.parse + self.comments + self.doc_build + self.render + self.post_process
    }
}

/// Accumulates `Instant` checkpoints during formatting, then finalizes into `Timings`.
pub struct TimingsBuilder {
    start: Instant,
    after_parse: Option<Instant>,
    after_comments: Option<Instant>,
    after_doc_build: Option<Instant>,
    after_render: Option<Instant>,
}

impl Default for TimingsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TimingsBuilder {
    pub fn new() -> Self {
        TimingsBuilder {
            start: Instant::now(),
            after_parse: None,
            after_comments: None,
            after_doc_build: None,
            after_render: None,
        }
    }

    pub fn mark_parse(&mut self) {
        self.after_parse = Some(Instant::now());
    }

    pub fn mark_comments(&mut self) {
        self.after_comments = Some(Instant::now());
    }

    pub fn mark_doc_build(&mut self) {
        self.after_doc_build = Some(Instant::now());
    }

    pub fn mark_render(&mut self) {
        self.after_render = Some(Instant::now());
    }

    pub fn finish(self) -> Timings {
        let end = Instant::now();
        let after_parse = self.after_parse.unwrap_or(self.start);
        let after_comments = self.after_comments.unwrap_or(after_parse);
        let after_doc_build = self.after_doc_build.unwrap_or(after_comments);
        let after_render = self.after_render.unwrap_or(after_doc_build);

        Timings {
            parse: after_parse.duration_since(self.start),
            comments: after_comments.duration_since(after_parse),
            doc_build: after_doc_build.duration_since(after_comments),
            render: after_render.duration_since(after_doc_build),
            post_process: end.duration_since(after_render),
        }
    }
}
