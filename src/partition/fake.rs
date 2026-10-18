//! A runner that runs nothing, for tests.

use std::cell::RefCell;
use std::collections::VecDeque;

use super::{Out, Runner};

/// Runs nothing: answers from rules (the first whose text is in "prog args" wins) and
/// remembers every call. Unlisted commands succeed with no output.
pub struct Fake {
    pub calls: RefCell<Vec<String>>,
    pub rules: Vec<(String, Out)>,
    /// Answers handed out one by one for commands containing the text, before `rules`
    pub queues: RefCell<Vec<(String, VecDeque<Out>)>>,
}

impl Fake {
    pub fn new(rules: Vec<(&str, Out)>) -> Fake {
        Fake { calls: Default::default(), queues: Default::default(), rules: rules.into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
    }

    pub fn then(self, key: &str, answers: Vec<Out>) -> Fake {
        self.queues.borrow_mut().push((key.to_string(), answers.into()));
        self
    }

    pub fn called(&self, what: &str) -> bool {
        self.calls.borrow().iter().any(|c| c.contains(what))
    }

    pub fn count(&self, what: &str) -> usize {
        self.calls.borrow().iter().filter(|c| c.contains(what)).count()
    }

    pub fn index(&self, what: &str) -> usize {
        self.calls.borrow().iter().position(|c| c.contains(what)).unwrap_or_else(|| panic!("{what} was never called: {:?}", self.calls.borrow()))
    }
}

impl Runner for Fake {
    fn run(&self, prog: &str, args: &[&str]) -> Out {
        let line = format!("{prog} {}", args.join(" "));
        self.calls.borrow_mut().push(line.clone());
        for (k, q) in self.queues.borrow_mut().iter_mut() {
            if line.contains(k.as_str()) {
                if let Some(o) = q.pop_front() {
                    return o;
                }
            }
        }
        self.rules.iter().find(|(k, _)| line.contains(k.as_str())).map(|(_, o)| o.clone()).unwrap_or_else(|| Out::success(""))
    }

    fn run_as_user(&self, args: &[&str]) -> Out {
        self.run("as-user", args)
    }
}
