//! Good news and gentle hints the worker tells the interface, as a toast.
//!
//! Like a [`Failure`](crate::failure::Failure), a notice is worded only when
//! the interface shows it, in the reader's language, so the worker never
//! writes a sentence. Names and paths stay as they are, inside the
//! translated sentence.

use std::borrow::Cow;

use crate::i18n::fill;

/// Something worth a calm toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// A file was posted to the conversation.
    Uploaded { name: String },
    /// A download was written to this path.
    Saved { path: String },
    /// A browser sign-in link needs one more step in the browser (SSO and
    /// the like) before it can sign in to a workspace.
    BrowserStep,
    /// The demo pretends to upload the file at this path.
    DemoUpload { path: String },
    /// The demo pretends to download this file.
    DemoSave { name: String },
    /// The demo pretends to open this file.
    DemoOpen { name: String },
}

impl Notice {
    /// The notice in a sentence, in the interface's language.
    pub fn message(&self) -> String {
        self.worded(&crate::i18n::t)
    }

    /// The words, through the translator `t`. Taking it as an argument lets
    /// the tests ask for every language without touching the process-wide
    /// one; it is called `t` so the catalog check finds these messages.
    fn worded(&self, t: &dyn Fn(&'static str) -> Cow<'static, str>) -> String {
        match self {
            Self::Uploaded { name } => fill(&t("Uploaded {name}"), &[("name", name)]),
            Self::Saved { path } => fill(&t("Saved {path}"), &[("path", path)]),
            Self::BrowserStep => {
                t("A workspace needs one more step in the browser; paste the new link it offers.")
                    .into_owned()
            }
            Self::DemoUpload { path } => fill(&t("Demo: would upload {path}"), &[("path", path)]),
            Self::DemoSave { name } => fill(&t("Demo: would save {name}"), &[("name", name)]),
            Self::DemoOpen { name } => fill(&t("Demo: would play {name}"), &[("name", name)]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;

    fn english(text: &'static str) -> Cow<'static, str> {
        fastframe_i18n::gettext(Locale::English, text)
    }

    fn dutch(text: &'static str) -> Cow<'static, str> {
        fastframe_i18n::gettext(Locale::Dutch, text)
    }

    #[test]
    fn every_notice_has_words_in_every_language() {
        let every = [
            Notice::Uploaded {
                name: "a.png".into(),
            },
            Notice::Saved {
                path: "/home/ann/Downloads/a.png".into(),
            },
            Notice::BrowserStep,
            Notice::DemoUpload {
                path: "/tmp/a.png".into(),
            },
            Notice::DemoSave {
                name: "a.png".into(),
            },
            Notice::DemoOpen {
                name: "a.mp3".into(),
            },
        ];
        for notice in every {
            let en = notice.worded(&english);
            let nl = notice.worded(&dutch);
            assert!(!en.is_empty() && !nl.is_empty(), "{notice:?}");
            assert_ne!(en, nl, "{notice:?} is not translated");
            assert!(!en.contains('{') && !nl.contains('{'), "{notice:?}: a hole");
        }
        assert_eq!(
            Notice::Uploaded {
                name: "{name}.png".into()
            }
            .worded(&english),
            "Uploaded {name}.png"
        );
    }
}
