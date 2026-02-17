use crate::{editor::state::SelectNextState, scroll::ScrollPosition, stoat::KeyContext};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use text::{Anchor, Selection, TransactionId};

type Selections = Arc<[Selection<Anchor>]>;

/// Tracks selections associated with buffer transactions, plus standalone
/// selection and app-state undo/redo stacks.
#[derive(Default)]
pub struct SelectionHistory {
    selections_by_transaction: HashMap<TransactionId, (Selections, Option<Selections>)>,
    selection_undo_stack: VecDeque<SelectionHistoryEntry>,
    selection_redo_stack: VecDeque<SelectionHistoryEntry>,
}

const SELECTION_HISTORY_CAP: usize = 256;

impl SelectionHistory {
    pub fn insert_transaction(&mut self, tx_id: TransactionId, before: Selections) {
        self.selections_by_transaction.insert(tx_id, (before, None));
    }

    pub fn set_after_selections(&mut self, tx_id: TransactionId, after: Selections) {
        if let Some(entry) = self.selections_by_transaction.get_mut(&tx_id) {
            entry.1 = Some(after);
        }
    }

    pub fn transaction(&self, tx_id: TransactionId) -> Option<&(Selections, Option<Selections>)> {
        self.selections_by_transaction.get(&tx_id)
    }

    pub fn push_selection_undo(&mut self, entry: SelectionHistoryEntry) {
        if self.selection_undo_stack.len() >= SELECTION_HISTORY_CAP {
            self.selection_undo_stack.pop_front();
        }
        self.selection_undo_stack.push_back(entry);
        self.selection_redo_stack.clear();
    }

    pub fn pop_selection_undo(&mut self) -> Option<SelectionHistoryEntry> {
        self.selection_undo_stack.pop_back()
    }

    pub fn push_selection_redo(&mut self, entry: SelectionHistoryEntry) {
        if self.selection_redo_stack.len() >= SELECTION_HISTORY_CAP {
            self.selection_redo_stack.pop_front();
        }
        self.selection_redo_stack.push_back(entry);
    }

    pub fn pop_selection_redo(&mut self) -> Option<SelectionHistoryEntry> {
        self.selection_redo_stack.pop_back()
    }
}

/// Entry in the selection undo/redo stacks.
#[derive(Clone)]
pub struct SelectionHistoryEntry {
    pub selections: Arc<[Selection<Anchor>]>,
    pub select_next_state: Option<SelectNextState>,
    pub select_prev_state: Option<SelectNextState>,
}

/// Snapshot of the full app state for app-level undo/redo.
#[derive(Clone)]
pub struct AppStateSnapshot {
    pub mode: String,
    pub key_context: KeyContext,
    pub selections: Arc<[Selection<Anchor>]>,
    pub select_next_state: Option<SelectNextState>,
    pub select_prev_state: Option<SelectNextState>,
    pub scroll: ScrollPosition,
}

const APP_STATE_HISTORY_CAP: usize = 64;

#[derive(Default)]
pub struct AppStateHistory {
    undo_stack: VecDeque<AppStateSnapshot>,
    redo_stack: VecDeque<AppStateSnapshot>,
}

impl AppStateHistory {
    pub fn push_undo(&mut self, snapshot: AppStateSnapshot) {
        if self.undo_stack.len() >= APP_STATE_HISTORY_CAP {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(snapshot);
        self.redo_stack.clear();
    }

    pub fn pop_undo(&mut self) -> Option<AppStateSnapshot> {
        self.undo_stack.pop_back()
    }

    pub fn push_redo(&mut self, snapshot: AppStateSnapshot) {
        if self.redo_stack.len() >= APP_STATE_HISTORY_CAP {
            self.redo_stack.pop_front();
        }
        self.redo_stack.push_back(snapshot);
    }

    pub fn pop_redo(&mut self) -> Option<AppStateSnapshot> {
        self.redo_stack.pop_back()
    }
}
