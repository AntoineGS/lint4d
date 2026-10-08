#![allow(dead_code)]
use pascal_core::compile_time::*;
use pascal_core::conditional::Truth;
use pascal_core::{LoadedSource, NoCancellation, SourceId, SourceRevision};
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, LayoutContext, LayoutPlatform,
    LayoutSettings,
};

pub fn context(platform: LayoutPlatform) -> ConditionalContext {
    ConditionalContext::default()
        .with_compiler_version(CompilerVersion::new(37, 0))
        .with_layout(LayoutContext {
            platform: Some(platform),
            defaults: LayoutSettings {
                long_strings: ConditionalFact::True,
                real_compatibility: ConditionalFact::False,
                ..LayoutSettings::default()
            },
        })
}

pub fn loaded(path: &str, text: &str) -> LoadedSource {
    LoadedSource {
        id: SourceId::new(format!("/fixture/{path}")),
        path: format!("/fixture/{path}").into(),
        bytes: text.as_bytes().into(),
        decoded_text: Some(text.into()),
        revision: SourceRevision::Overlay {
            version: 1,
            content_hash: pascal_project::content_hash_bytes(text.as_bytes()),
        },
    }
}

pub struct Harness {
    pub state: CompileTimeState,
    pub root: LoadedSource,
    text: String,
    finished: bool,
    budget: CompileTimeBudget<'static>,
}
impl Harness {
    pub fn stream(text: &str, context: ConditionalContext) -> Self {
        let root = loaded("T.pas", text);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let state = CompileTimeState::new(&context, &root, &mut budget).unwrap();
        let mut h = Self {
            state,
            root,
            text: String::new(),
            finished: false,
            budget,
        };
        h.feed(text);
        h
    }
    pub fn plain(text: &str, context: ConditionalContext) -> Self {
        let mut h = Self::stream(text, context);
        h.finish();
        h
    }
    pub fn feed(&mut self, text: &str) {
        self.feed_activity(text, Truth::True);
    }
    pub fn feed_activity(&mut self, text: &str, activity: Truth) {
        assert!(!self.finished);
        self.state
            .feed(
                SourceFragment {
                    source: &self.root.id,
                    occurrence: OccurrenceId(0),
                    physical_start: self.text.len(),
                    text,
                },
                activity,
                &mut self.budget,
            )
            .unwrap();
        self.text.push_str(text);
    }
    pub fn finish(&mut self) {
        assert!(!self.finished);
        self.state.finish_root(&mut self.budget).unwrap();
        self.finished = true;
    }
    pub fn with_layout(&mut self, body: &str) {
        self.with_layout_activity(body, Truth::True);
    }
    pub fn with_layout_activity(&mut self, body: &str, activity: Truth) {
        assert!(!self.finished);
        self.state
            .apply_layout_directive(body, activity, &mut self.budget)
            .unwrap();
    }
    pub fn size(&self, name: &str) -> Option<u64> {
        self.at(self.text.len(), name)
    }
    pub fn size_at(&self, marker: &str, name: &str) -> Option<u64> {
        assert_eq!(self.text.matches(marker).count(), 1);
        self.at(self.text.find(marker).unwrap(), name)
    }
    fn at(&self, offset: usize, name: &str) -> Option<u64> {
        let site = self.state.site_at(&self.root.id, OccurrenceId(0), offset)?;
        self.state
            .layout_name(
                site,
                name,
                &mut CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation),
            )
            .known()
            .map(|v| v.storage.size)
    }
}
