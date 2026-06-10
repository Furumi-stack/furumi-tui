use serde::Deserialize;

/// Semantic commands the user can trigger. Raw key events are translated into
/// these by the keymap; views and `update()` never see raw keys.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum Action {
    Quit,
    NextTab,
    PrevTab,
    GoToTab(usize),
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    PageUp,
    PageDown,
    SelectFirst,
    SelectLast,
    Select,
    Back,
    PlayPause,
    NextTrack,
    PrevTrack,
    SeekForward { seconds: u32 },
    SeekBackward { seconds: u32 },
    VolumeUp,
    VolumeDown,
    ToggleShuffle,
    CycleRepeat,
    ToggleLike,
    QueueAddNext,
    QueueAddLast,
    ToggleHelp,
    ToggleViewMode,
    OpenCommandLine,
    Logout,
}

impl Action {
    pub fn describe(&self) -> String {
        match self {
            Action::Quit => "Quit".into(),
            Action::NextTab => "Next tab".into(),
            Action::PrevTab => "Previous tab".into(),
            Action::GoToTab(i) => format!("Go to tab {}", i + 1),
            Action::MoveUp => "Move up".into(),
            Action::MoveDown => "Move down".into(),
            Action::MoveLeft => "Move left".into(),
            Action::MoveRight => "Move right".into(),
            Action::PageUp => "Page up".into(),
            Action::PageDown => "Page down".into(),
            Action::SelectFirst => "Jump to first item".into(),
            Action::SelectLast => "Jump to last item".into(),
            Action::Select => "Open / activate".into(),
            Action::Back => "Go back / close".into(),
            Action::PlayPause => "Play / pause".into(),
            Action::NextTrack => "Next track".into(),
            Action::PrevTrack => "Previous track".into(),
            Action::SeekForward { seconds } => format!("Seek forward {seconds}s"),
            Action::SeekBackward { seconds } => format!("Seek backward {seconds}s"),
            Action::VolumeUp => "Volume up".into(),
            Action::VolumeDown => "Volume down".into(),
            Action::ToggleShuffle => "Toggle shuffle".into(),
            Action::CycleRepeat => "Cycle repeat mode".into(),
            Action::ToggleLike => "Like / unlike".into(),
            Action::QueueAddNext => "Queue: add next".into(),
            Action::QueueAddLast => "Queue: add to end".into(),
            Action::ToggleHelp => "Show / hide keybindings".into(),
            Action::ToggleViewMode => "Toggle tiles / table view".into(),
            Action::OpenCommandLine => "Open command line (:/name searches)".into(),
            Action::Logout => "Sign out".into(),
        }
    }
}
