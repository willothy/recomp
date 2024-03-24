use std::{ptr::NonNull, sync::Arc};

use x11rb::xcb_ffi::XCBConnection;
use x11rb_async::blocking::BlockingConnection;

#[derive(Clone)]
pub struct XConn(Arc<XConnInner>);

struct XConnInner {
    raw: Arc<XCBConnection>,
    conn: BlockingConnection<XCBConnection>,
    screen_num: usize,
}

impl XConn {
    pub fn new(raw: Arc<XCBConnection>, screen_num: usize) -> Self {
        let conn = BlockingConnection::new(Arc::clone(&raw));
        Self(Arc::new(XConnInner {
            raw,
            conn,
            screen_num,
        }))
    }

    pub fn screen(&self) -> usize {
        self.0.screen_num
    }

    pub fn as_raw_connection(&self) -> NonNull<std::ffi::c_void> {
        // Safety: XCB should hopefully never hand us a null pointer.
        unsafe { NonNull::new_unchecked(self.0.raw.get_raw_xcb_connection()) }
    }
}

impl std::ops::Deref for XConn {
    type Target = BlockingConnection<XCBConnection>;

    fn deref(&self) -> &Self::Target {
        &self.0.conn
    }
}
