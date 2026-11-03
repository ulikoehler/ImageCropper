//! Filesystem-watcher event filtering: the rescan reads every image file,
//! so read-side access events must be ignored or each rescan re-triggers
//! itself in an infinite loop.

use imagecropper::server::watch::event_affects_listing;
use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, EventKind, MetadataKind, ModifyKind, RemoveKind, RenameMode};

#[test]
fn read_side_access_events_are_ignored() {
    for kind in [
        EventKind::Access(AccessKind::Read),
        EventKind::Access(AccessKind::Open(AccessMode::Any)),
        EventKind::Access(AccessKind::Open(AccessMode::Read)),
        EventKind::Access(AccessKind::Close(AccessMode::Read)),
        EventKind::Access(AccessKind::Any),
        EventKind::Access(AccessKind::Other),
    ] {
        assert!(!event_affects_listing(&kind), "{kind:?} must be ignored");
    }
}

#[test]
fn listing_affecting_events_pass() {
    for kind in [
        EventKind::Create(CreateKind::File),
        EventKind::Create(CreateKind::Folder),
        EventKind::Create(CreateKind::Any),
        EventKind::Remove(RemoveKind::File),
        EventKind::Remove(RemoveKind::Folder),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
        EventKind::Modify(ModifyKind::Data(DataChange::Any)),
        EventKind::Modify(ModifyKind::Any),
        EventKind::Access(AccessKind::Close(AccessMode::Write)),
        EventKind::Any,
        EventKind::Other,
    ] {
        assert!(event_affects_listing(&kind), "{kind:?} must stay relevant");
    }
}

#[test]
fn metadata_only_changes_are_ignored() {
    for kind in [
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
    ] {
        assert!(!event_affects_listing(&kind), "{kind:?} must be ignored");
    }
}
