// SPDX-License-Identifier: AGPL-3.0-or-later

//! AppKit drops into the existing winit content view.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use i_slint_core::input::BackendDragEvent;
use i_slint_core::items::{AllowedDragActions, DragAction, DropEvent};
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, ProtocolObject, Sel};
use objc2::sel;
use objc2_app_kit::{
    NSDragOperation, NSDraggingInfo, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeString,
    NSView,
};
use objc2_foundation::{NSArray, NSPoint, NSString, NSURL};
use slint::private_unstable_api::re_exports::{DataTransfer, WindowAdapter, WindowInner};
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{DropPosition, DroppedText, FileDrop, WinitWindow};

type DraggingInfo = ProtocolObject<dyn NSDraggingInfo>;

struct Receiver {
    view: *const NSView,
    adapter: Weak<dyn WindowAdapter>,
    files: Rc<FileDrop>,
    drag: RefCell<Option<(isize, DataTransfer)>>,
}

thread_local! {
    static RECEIVER: RefCell<Option<Rc<Receiver>>> = const { RefCell::new(None) };
}

const COPY: AllowedDragActions = AllowedDragActions {
    copy: true,
    move_: false,
    link: false,
};

fn view(window: &WinitWindow) -> Option<&NSView> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: winit's borrowed window handle keeps its AppKit view alive.
    Some(unsafe { handle.ns_view.cast::<NSView>().as_ref() })
}

pub(super) fn install(native: &WinitWindow, window: &slint::Window, files: Rc<FileDrop>) -> bool {
    let Some(view) = view(native) else {
        return false;
    };
    let object: &AnyObject = view.as_ref();
    let original = object.class();
    let name = c"ConcatDropTargetView";
    if AnyClass::get(name) != Some(original) {
        let class = match AnyClass::get(name) {
            Some(existing) => existing,
            None => {
                let Some(mut class) = ClassBuilder::new(name, original) else {
                    return false;
                };
                // SAFETY: these signatures are the NSDraggingDestination methods on NSView.
                unsafe {
                    class.add_method(
                        sel!(draggingEntered:),
                        drag_over as extern "C-unwind" fn(_, _, _) -> _,
                    );
                    class.add_method(
                        sel!(draggingUpdated:),
                        drag_over as extern "C-unwind" fn(_, _, _) -> _,
                    );
                    class.add_method(
                        sel!(draggingExited:),
                        drag_exited as extern "C-unwind" fn(_, _, _),
                    );
                    class.add_method(
                        sel!(prepareForDragOperation:),
                        prepare_drop as extern "C-unwind" fn(_, _, _) -> _,
                    );
                    class.add_method(
                        sel!(performDragOperation:),
                        perform_drop as extern "C-unwind" fn(_, _, _) -> _,
                    );
                }
                class.register()
            }
        };
        if class.superclass() != Some(original) || class.instance_size() != original.instance_size()
        {
            return false;
        }
        // SAFETY: only the main thread touches the view. The subclass inherits
        // winit's original view and only adds drag methods, with no new ivars.
        let previous = unsafe { AnyObject::set_class(object, class) };
        debug_assert_eq!(original, previous);
    }

    let adapter = WindowInner::from_pub(window).window_adapter();
    RECEIVER.replace(Some(Rc::new(Receiver {
        view,
        adapter: Rc::downgrade(&adapter),
        files,
        drag: RefCell::new(None),
    })));
    view.registerForDraggedTypes(&NSArray::from_slice(&[
        string_type(),
        file_url_type(),
        legacy_files_type(),
    ]));
    true
}

fn string_type() -> &'static NSString {
    // SAFETY: AppKit supplies this constant for the life of the process.
    unsafe { NSPasteboardTypeString }
}

fn file_url_type() -> &'static NSString {
    // SAFETY: AppKit supplies this constant for the life of the process.
    unsafe { NSPasteboardTypeFileURL }
}

#[allow(deprecated)]
fn legacy_files_type() -> &'static NSString {
    // SAFETY: AppKit supplies this constant for the life of the process.
    unsafe { objc2_app_kit::NSFilenamesPboardType }
}

fn receiver(view: &NSView) -> Option<Rc<Receiver>> {
    RECEIVER.with_borrow(|entry| {
        entry
            .as_ref()
            .filter(|receiver| std::ptr::eq(receiver.view, view))
            .cloned()
    })
}

fn read_data(board: &NSPasteboard) -> DataTransfer {
    let mut paths = Vec::new();
    if let Some(items) = board.pasteboardItems() {
        for item in items.iter() {
            if let Some(value) = item.stringForType(file_url_type())
                && let Some(url) = NSURL::URLWithString(&value)
                && url.isFileURL()
                && let Some(path) = url.path()
            {
                paths.push(PathBuf::from(path.to_string()));
            }
        }
    }
    if paths.is_empty()
        && let Some(list) = board.propertyListForType(legacy_files_type())
        && let Some(list) = list.downcast_ref::<NSArray<AnyObject>>()
    {
        paths.extend(list.iter().filter_map(|item| {
            item.downcast_ref::<NSString>()
                .map(|path| PathBuf::from(path.to_string()))
        }));
    }

    let mut data = DataTransfer::default();
    if !paths.is_empty() {
        data.set_file_paths(paths);
    } else if let Some(text) = board.stringForType(string_type()) {
        let text = text.to_string();
        if !text.is_empty() {
            data.set_user_data(Rc::new(DroppedText(text.into())));
        }
    }
    data
}

fn logical_position(
    view: &NSView,
    window: &slint::Window,
    location: NSPoint,
) -> Option<slint::LogicalPosition> {
    let backing = view.window()?.backingScaleFactor();
    let point = view.convertPoint_fromView(location, None);
    let scale = backing / f64::from(window.scale_factor());
    Some(slint::LogicalPosition::new(
        (point.x * scale) as f32,
        (point.y * scale) as f32,
    ))
}

pub(super) fn mouse_position(native: &WinitWindow, window: &slint::Window) -> DropPosition {
    let view = view(native)?;
    let point = logical_position(
        view,
        window,
        view.window()?.mouseLocationOutsideOfEventStream(),
    )?;
    Some((point.x, point.y))
}

impl Receiver {
    fn event(
        &self,
        view: &NSView,
        window: &slint::Window,
        info: &DraggingInfo,
    ) -> Option<DropEvent> {
        if !info
            .draggingSourceOperationMask()
            .contains(NSDragOperation::Copy)
        {
            return None;
        }
        let sequence = info.draggingSequenceNumber();
        let cached = self
            .drag
            .borrow()
            .as_ref()
            .filter(|(current, _)| *current == sequence)
            .map(|(_, data)| data.clone());
        let data = cached.unwrap_or_else(|| {
            let data = read_data(&info.draggingPasteboard());
            self.drag.replace(Some((sequence, data.clone())));
            data
        });
        let position = logical_position(view, window, info.draggingLocation())?;
        let mut event = DropEvent::default();
        event.data = data;
        event.position = position;
        event.proposed_action = DragAction::Copy;
        Some(event)
    }

    fn leave(&self) {
        self.drag.replace(None);
        if let Some(adapter) = self.adapter.upgrade() {
            WindowInner::from_pub(adapter.window()).process_drag_event(BackendDragEvent::Leave);
        }
    }
}

extern "C-unwind" fn drag_over(view: &NSView, _: Sel, info: &DraggingInfo) -> NSDragOperation {
    let Some(receiver) = receiver(view) else {
        return NSDragOperation::None;
    };
    let Some(adapter) = receiver.adapter.upgrade() else {
        return NSDragOperation::None;
    };
    let window = adapter.window();
    let Some(event) = receiver.event(view, window, info) else {
        receiver.leave();
        return NSDragOperation::None;
    };
    let files = event.data.has_file_paths();
    let accepted = WindowInner::from_pub(window).process_drag_event(BackendDragEvent::Move {
        event,
        allowed: COPY,
    }) == Some(DragAction::Copy);
    if accepted || files {
        NSDragOperation::Copy
    } else {
        NSDragOperation::None
    }
}

extern "C-unwind" fn drag_exited(view: &NSView, _: Sel, _: Option<&DraggingInfo>) {
    if let Some(receiver) = receiver(view) {
        receiver.leave();
    }
}

extern "C-unwind" fn prepare_drop(view: &NSView, selector: Sel, info: &DraggingInfo) -> Bool {
    Bool::new(drag_over(view, selector, info) == NSDragOperation::Copy)
}

extern "C-unwind" fn perform_drop(view: &NSView, _: Sel, info: &DraggingInfo) -> Bool {
    let Some(receiver) = receiver(view) else {
        return Bool::NO;
    };
    let Some(adapter) = receiver.adapter.upgrade() else {
        return Bool::NO;
    };
    let window = adapter.window();
    let Some(event) = receiver.event(view, window, info) else {
        receiver.leave();
        return Bool::NO;
    };
    let files = event.data.file_paths().map_or_else(
        |_| Vec::new(),
        |paths| paths.map(std::path::Path::to_path_buf).collect::<Vec<_>>(),
    );
    let position = Some((event.position.x, event.position.y));
    let accepted = WindowInner::from_pub(window).process_drag_event(BackendDragEvent::Drop {
        event,
        allowed: COPY,
    }) == Some(DragAction::Copy);
    receiver.leave();

    if !accepted && !files.is_empty() {
        (receiver.files)(files, position);
        return Bool::YES;
    }
    Bool::new(accepted)
}
