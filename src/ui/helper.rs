//! rustyline helper：把 Ctrl-C 灰提示和补全提供者接到编辑器上。
//! 命令语义与候选生成在 `commands`，这里只负责 rustyline 类型适配。

use crate::completion::CompletionProvider;
use crate::render;
use crate::ui::hint::{HintArm, CTRL_C_HINT};
use std::borrow::Cow;
use std::sync::Arc;

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

pub(super) struct ReplHelper {
    hint: Arc<HintArm>,
    completer: Arc<dyn CompletionProvider>,
}

impl ReplHelper {
    pub(super) fn new(hint: Arc<HintArm>) -> Self {
        Self {
            hint,
            completer: Arc::new(crate::completion::NoCompletion),
        }
    }

    pub(super) fn set_completer(&mut self, completer: Arc<dyn CompletionProvider>) {
        self.completer = completer;
    }
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let completion = self.completer.complete(line, pos);
        Ok((
            completion.start,
            completion
                .candidates
                .into_iter()
                .map(|candidate| Pair {
                    display: candidate.display,
                    replacement: candidate.replacement,
                })
                .collect(),
        ))
    }
}

impl Hinter for ReplHelper {
    type Hint = String;

    fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        let empty = line.is_empty();
        let show = empty && self.hint.armed();
        self.hint.set_line_empty(empty);
        self.hint.mark_layout_hint(show);
        show.then(|| CTRL_C_HINT.to_string())
    }
}

impl Highlighter for ReplHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(render::input(line))
        }
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(render::prompt(prompt))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(render::dim(hint))
    }

    fn highlight_char(&self, line: &str, _pos: usize, _forced: bool) -> bool {
        // 非空行需要整行重绘：rustyline 在 `highlight_char` 为 false 时直接写字符，
        // 会绕过 `highlight`，输入行就不会着色。
        !line.is_empty()
    }
}

impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{Completion, NoCompletion};

    struct Fixed(Completion);

    impl CompletionProvider for Fixed {
        fn complete(&self, _line: &str, _pos: usize) -> Completion {
            self.0.clone()
        }
    }

    #[test]
    fn prompt_input_and_hint_are_styled() {
        let helper = ReplHelper::new(Arc::new(HintArm::new(false)));
        assert_eq!(
            helper.highlight_prompt("rho> ", true).as_ref(),
            "\x1b[1;36mrho> \x1b[0m"
        );
        assert_eq!(helper.highlight("ls", 2).as_ref(), "\x1b[36mls\x1b[0m");
        assert_eq!(helper.highlight("", 0).as_ref(), "");
        assert_eq!(helper.highlight_hint("hint").as_ref(), "\x1b[2mhint\x1b[0m");
        assert!(helper.highlight_char("x", 1, false));
        assert!(!helper.highlight_char("", 0, false));
    }

    #[test]
    fn completer_maps_candidates_to_rustyline_pairs() {
        let hint = Arc::new(HintArm::new(false));
        let mut helper = ReplHelper::new(hint);
        helper.set_completer(Arc::new(Fixed(Completion {
            start: 3,
            candidates: vec![crate::completion::Candidate {
                display: "display".into(),
                replacement: "replacement".into(),
            }],
        })));

        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, pairs) = helper.complete("/mo", 3, &ctx).unwrap();
        assert_eq!(start, 3);
        assert_eq!(pairs[0].display, "display");
        assert_eq!(pairs[0].replacement, "replacement");
    }

    #[test]
    fn default_completer_has_no_candidates() {
        let hint = Arc::new(HintArm::new(false));
        let helper = ReplHelper {
            hint,
            completer: Arc::new(NoCompletion),
        };
        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);
        assert!(helper.complete("/mo", 3, &ctx).unwrap().1.is_empty());
    }
}
