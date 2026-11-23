//! A table of slices that is its own data source. The slice list renames and deletes slices.
//! The play list reorders and removes them and takes slices dragged in from the slice list.
//! What the user did is queued and read on the app's tick.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
  NSDragOperation, NSDraggingInfo, NSEvent, NSPasteboardItem, NSPasteboardWriting, NSScrollView,
  NSTableColumn, NSTableView, NSTableViewDataSource, NSTableViewDropOperation,
};
use objc2_foundation::{
  NSArray, NSIndexSet, NSInteger, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

/// Carries a slice's name from either table to the play list.
const SLICE_TYPE: &str = "com.excsn.mac.fastflow.slice";
const DELETE_KEYS: [u16; 2] = [51, 117];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
  Slices,
  Plays,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListEvent {
  Rename {
    from: String,
    to: String,
  },
  Delete(String),
  /// Put the slice before row `at` of the play list.
  Place {
    name: String,
    at: usize,
  },
}

pub struct ListIvars {
  kind: Kind,
  /// One string per column for each row.
  rows: RefCell<Vec<Vec<String>>>,
  /// The slice each row shows.
  names: RefCell<Vec<String>>,
  events: RefCell<Vec<ListEvent>>,
}

define_class!(
    // SAFETY: NSTableView has no subclassing requirements for the overridden methods and
    // ListTable does not implement Drop.
    #[unsafe(super(NSTableView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastflowListTable"]
    #[ivars = ListIvars]
    pub struct ListTable;

    impl ListTable {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if DELETE_KEYS.contains(&event.keyCode()) && let Some(name) = self.selected_name() {
                self.ivars().events.borrow_mut().push(ListEvent::Delete(name));
                return;
            }
            unsafe { msg_send![super(self), keyDown: event] }
        }
    }

    unsafe impl NSObjectProtocol for ListTable {}

    unsafe impl NSTableViewDataSource for ListTable {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().rows.borrow().len() as NSInteger
        }

        #[unsafe(method_id(tableView:objectValueForTableColumn:row:))]
        fn object_value(
            &self,
            table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<AnyObject>> {
            self.cell(table, column, row)
        }

        #[unsafe(method(tableView:setObjectValue:forTableColumn:row:))]
        fn set_object_value(
            &self,
            _table: &NSTableView,
            value: Option<&AnyObject>,
            _column: Option<&NSTableColumn>,
            row: NSInteger,
        ) {
            let Some(to) = value.and_then(|v| v.downcast_ref::<NSString>()) else {
                return;
            };
            let Some(from) = self.ivars().names.borrow().get(row as usize).cloned() else {
                return;
            };
            let to = to.to_string().trim().to_owned();
            if to != from && !to.is_empty() {
                self.ivars().events.borrow_mut().push(ListEvent::Rename { from, to });
            }
        }

        #[unsafe(method_id(tableView:pasteboardWriterForRow:))]
        fn pasteboard_writer(
            &self,
            _table: &NSTableView,
            row: NSInteger,
        ) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
            self.writer(row)
        }

        #[unsafe(method(tableView:validateDrop:proposedRow:proposedDropOperation:))]
        fn validate_drop(
            &self,
            _table: &NSTableView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            row: NSInteger,
            _op: NSTableViewDropOperation,
        ) -> NSDragOperation {
            if self.ivars().kind != Kind::Plays || dragged_name(info).is_none() {
                return NSDragOperation::None;
            }
            self.setDropRow_dropOperation(row.max(0), NSTableViewDropOperation::Above);
            if self.is_source(info) { NSDragOperation::Move } else { NSDragOperation::Copy }
        }

        #[unsafe(method(tableView:acceptDrop:row:dropOperation:))]
        fn accept_drop(
            &self,
            _table: &NSTableView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            row: NSInteger,
            _op: NSTableViewDropOperation,
        ) -> Bool {
            let Some(name) = dragged_name(info) else { return Bool::NO };
            let at = row.max(0) as usize;
            self.ivars().events.borrow_mut().push(ListEvent::Place { name, at });
            Bool::YES
        }
    }
);

fn dragged_name(info: &ProtocolObject<dyn NSDraggingInfo>) -> Option<String> {
  let board = info.draggingPasteboard();
  board
    .stringForType(&NSString::from_str(SLICE_TYPE))
    .map(|s| s.to_string())
}

impl ListTable {
  fn cell(
    &self,
    table: &NSTableView,
    column: Option<&NSTableColumn>,
    row: NSInteger,
  ) -> Option<Retained<AnyObject>> {
    let c = table.columnWithIdentifier(&column?.identifier());
    let rows = self.ivars().rows.borrow();
    let text = rows.get(row as usize)?.get(c as usize)?;
    // SAFETY: every NSString is an AnyObject.
    Some(unsafe { Retained::cast_unchecked(NSString::from_str(text)) })
  }

  fn writer(&self, row: NSInteger) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
    let name = self.ivars().names.borrow().get(row as usize)?.clone();
    let item = NSPasteboardItem::new();
    item.setString_forType(&NSString::from_str(&name), &NSString::from_str(SLICE_TYPE));
    Some(ProtocolObject::from_retained(item))
  }

  /// A table with one column per `(title, width)`. The first column holds the slice's name and
  /// is editable in the slice list.
  pub fn new(mtm: MainThreadMarker, kind: Kind, columns: &[(&str, f64)]) -> Retained<ListTable> {
    let this = Self::alloc(mtm).set_ivars(ListIvars {
      kind,
      rows: RefCell::new(Vec::new()),
      names: RefCell::new(Vec::new()),
      events: RefCell::new(Vec::new()),
    });
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 100.0));
    let table: Retained<ListTable> = unsafe { msg_send![super(this), initWithFrame: frame] };
    for (i, (title, width)) in columns.iter().enumerate() {
      let column = NSTableColumn::initWithIdentifier(
        NSTableColumn::alloc(mtm),
        &NSString::from_str(&i.to_string()),
      );
      column.setTitle(&NSString::from_str(title));
      column.setWidth(*width);
      column.setEditable(i == 0 && kind == Kind::Slices);
      table.addTableColumn(&column);
    }
    table.setUsesAlternatingRowBackgroundColors(true);
    table.setAllowsMultipleSelection(false);
    table
      .setDraggingSourceOperationMask_forLocal(NSDragOperation::Copy | NSDragOperation::Move, true);
    if kind == Kind::Plays {
      let types = NSArray::from_retained_slice(&[NSString::from_str(SLICE_TYPE)]);
      table.registerForDraggedTypes(&types);
    }
    unsafe { table.setDataSource(Some(ProtocolObject::from_ref(&*table))) };
    table
  }

  /// `table` inside a bordered scroll view of `size`.
  pub fn scrolled(
    table: &ListTable,
    mtm: MainThreadMarker,
    size: (f64, f64),
  ) -> Retained<NSScrollView> {
    let scroll = NSScrollView::initWithFrame(
      NSScrollView::alloc(mtm),
      NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.0, size.1)),
    );
    scroll.setDocumentView(Some(table));
    scroll.setHasVerticalScroller(true);
    scroll.setBorderType(objc2_app_kit::NSBorderType::BezelBorder);
    scroll
      .widthAnchor()
      .constraintEqualToConstant(size.0)
      .setActive(true);
    scroll
      .heightAnchor()
      .constraintEqualToConstant(size.1)
      .setActive(true);
    scroll
  }

  /// Replaces the rows when they changed. `names[i]` is the slice row `i` shows.
  pub fn set_rows(&self, rows: Vec<Vec<String>>, names: Vec<String>) {
    let iv = self.ivars();
    if *iv.rows.borrow() == rows && *iv.names.borrow() == names {
      return;
    }
    *iv.rows.borrow_mut() = rows;
    *iv.names.borrow_mut() = names;
    self.reloadData();
  }

  pub fn take_events(&self) -> Vec<ListEvent> {
    std::mem::take(&mut *self.ivars().events.borrow_mut())
  }

  pub fn selected_name(&self) -> Option<String> {
    let row = self.selectedRow();
    (row >= 0)
      .then(|| self.ivars().names.borrow().get(row as usize).cloned())
      .flatten()
  }

  /// Selects the row showing `name`. `None` clears the selection.
  pub fn select_name(&self, name: Option<&str>) {
    let row = name.and_then(|n| self.ivars().names.borrow().iter().position(|r| r == n));
    match row {
      Some(r) => {
        self.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(r), false)
      }
      None => self.selectRowIndexes_byExtendingSelection(&NSIndexSet::new(), false),
    }
  }

  fn is_source(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    info.draggingSource().is_some_and(|s| {
      std::ptr::eq(
        &*s as *const AnyObject,
        self as *const ListTable as *const AnyObject,
      )
    })
  }
}
