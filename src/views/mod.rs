pub mod connections;
pub mod purposes;
pub mod selector;

// Re-export only the items directly held as fields on `App`; everything else
// is reached through the submodule paths to keep the `use` graph readable.
pub use connections::ConnectionsView;
pub use purposes::PurposesView;
pub use selector::{ConversationSelections, ModelSelector};
