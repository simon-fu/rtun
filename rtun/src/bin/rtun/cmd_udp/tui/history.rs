use std::collections::VecDeque;

use dialoguer::History;


pub struct InputHistory {
    max_entries: Option<usize>,
    deque: VecDeque<String>,
    no_duplicates: bool,
    skip_last_duplicate: bool,
}

impl InputHistory {
    /// Creates a new basic history value which has no limit on the number of
    /// entries and allows for duplicates.
    ///
    /// # Example
    ///
    /// A history with at most 8 entries and no duplicates:
    ///
    /// ```rs
    /// let mut history = BasicHistory::new().max_entries(8).no_duplicates(true);
    /// ```
    pub fn new() -> Self {
        Self {
            max_entries: None,
            deque: VecDeque::new(),
            no_duplicates: false,
            skip_last_duplicate: false,
        }
    }

    /// Limit the number of entries stored in the history.
    pub fn max_entries(self, max_entries: usize) -> Self {
        Self {
            max_entries: Some(max_entries),
            ..self
        }
    }

    /// Prevent duplicates in the history. This means that any previous entries
    /// that are equal to a new entry are removed before the new entry is added.
    pub fn no_duplicates(self, no_duplicates: bool) -> Self {
        Self {
            no_duplicates,
            ..self
        }
    }

    pub fn skip_last_duplicate(self, skip_last_duplicate: bool) -> Self {
        Self {
            skip_last_duplicate,
            ..self
        }
    }
}

impl<T: ToString> History<T> for InputHistory {
    fn read(&self, pos: usize) -> Option<String> {
        self.deque.get(pos).cloned()
    }

    fn write(&mut self, val: &T) {
        let val = val.to_string();

        if self.skip_last_duplicate {
            if let Some(last) = self.deque.front() {
                if *last == val {
                    return;
                }
            }
        }

        if self.no_duplicates {
            self.deque.retain(|v| v != &val);
        }

        self.deque.push_front(val);

        if let Some(max_entries) = self.max_entries {
            self.deque.truncate(max_entries);
        }
    }
}

impl Default for InputHistory {
    fn default() -> Self {
        Self::new()
    }
}
