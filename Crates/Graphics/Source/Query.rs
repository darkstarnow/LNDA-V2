#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, ComObject, E_INVALIDARG, HRESULT, Iid, Object, QUERY_HEAP, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x0d9658ae,
        0xed45,
        0x469e,
        [0xa6, 0x1d, 0x97, 0x0e, 0xc5, 0x83, 0xca, 0xb4],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Desc {
    kind: u32,
    count: u32,
    nodes: u32,
}

#[repr(C)]
pub(crate) struct QueryHeap {
    object: Object,
    desc: Desc,
}

unsafe impl ComObject for QueryHeap {
    const KIND: usize = QUERY_HEAP;
    const INTERFACES: &'static [Iid] = &HEAP_IIDS;
}

impl QueryHeap {
    fn new(desc: Desc) -> Box<Self> {
        Box::new(Self {
            object: Object::new(&FUNCTIONS),
            desc,
        })
    }

    pub(crate) extern "system" fn create(
        _: This,
        desc: *const Desc,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(&desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        tracing::debug!(?desc, "query heap");
        Self::new(desc).hand_out(iid, out)
    }
}

static FUNCTIONS: Table<24> = Table(object::pageable_table::<QueryHeap, QUERY_HEAP, 24>());
