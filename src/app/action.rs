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
    ToggleTrackSelection,
    OpenTrackInfo,
    QueueAddNext,
    QueueAddLast,
    RemoveFromQueue,
    ClearQueue,
    GoToRelease,
    AddToPlaylist,
    NewPlaylist,
    ToggleHelp,
    ToggleViewMode,
    OpenCommandLine,
    OpenSearch,
    EditSelected,
    DeleteSelected,
}

/// Help-window sections, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Playback,
    Queue,
    Library,
    Navigation,
    Search,
    System,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Playback,
        Category::Queue,
        Category::Library,
        Category::Navigation,
        Category::Search,
        Category::System,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Playback => "Playback",
            Category::Queue => "Queue & playlists",
            Category::Library => "Library",
            Category::Navigation => "Navigation",
            Category::Search => "Search & commands",
            Category::System => "System",
        }
    }
}

impl Action {
    pub fn category(&self) -> Category {
        match self {
            Action::PlayPause
            | Action::NextTrack
            | Action::PrevTrack
            | Action::SeekForward { .. }
            | Action::SeekBackward { .. }
            | Action::VolumeUp
            | Action::VolumeDown
            | Action::ToggleShuffle
            | Action::CycleRepeat => Category::Playback,
            Action::QueueAddNext
            | Action::QueueAddLast
            | Action::RemoveFromQueue
            | Action::ClearQueue
            | Action::AddToPlaylist
            | Action::NewPlaylist
            | Action::ToggleLike
            | Action::ToggleTrackSelection
            | Action::OpenTrackInfo => Category::Queue,
            Action::MoveUp
            | Action::MoveDown
            | Action::MoveLeft
            | Action::MoveRight
            | Action::PageUp
            | Action::PageDown
            | Action::SelectFirst
            | Action::SelectLast
            | Action::Select
            | Action::Back
            | Action::NextTab
            | Action::PrevTab
            | Action::GoToTab(_)
            | Action::GoToRelease
            | Action::ToggleViewMode => Category::Navigation,
            Action::EditSelected | Action::DeleteSelected => Category::Library,
            Action::OpenSearch | Action::OpenCommandLine => Category::Search,
            Action::ToggleHelp | Action::Quit => Category::System,
        }
    }

    /// The command-line equivalent shown in the help window, if any.
    pub fn command_hint(&self) -> Option<&'static str> {
        match self {
            Action::Quit => Some(":q"),
            Action::EditSelected => Some(":import <path> adds files"),
            Action::PlayPause => Some(":play"),
            Action::NextTrack => Some(":next"),
            Action::PrevTrack => Some(":prev"),
            Action::SeekForward { .. } | Action::SeekBackward { .. } => Some(":seek +30 | 1:30"),
            Action::VolumeUp | Action::VolumeDown => Some(":volume 0-100"),
            Action::ToggleShuffle => Some(":shuffle"),
            Action::CycleRepeat => Some(":repeat [off|one|all]"),
            Action::ClearQueue => Some(":clear"),
            Action::ToggleHelp => Some(":help"),
            Action::OpenSearch => Some("/text"),
            _ => None,
        }
    }

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
            Action::ToggleTrackSelection => "Track line selection".into(),
            Action::OpenTrackInfo => "Track info".into(),
            Action::QueueAddNext => "Queue: add next".into(),
            Action::QueueAddLast => "Queue: add to end".into(),
            Action::RemoveFromQueue => "Queue: remove selected".into(),
            Action::ClearQueue => "Queue: clear".into(),
            Action::GoToRelease => "Open the track's release".into(),
            Action::AddToPlaylist => "Add track to a playlist…".into(),
            Action::NewPlaylist => "Create a playlist".into(),
            Action::ToggleHelp => "Show / hide keybindings".into(),
            Action::ToggleViewMode => "Toggle tiles / table view".into(),
            Action::OpenCommandLine => "Command line (:help for commands)".into(),
            Action::OpenSearch => "Search artists, releases, tracks".into(),
            Action::EditSelected => "Edit the selected item".into(),
            Action::DeleteSelected => "Delete the selected item".into(),
        }
    }
}
