#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, COMMAND_SIGNATURE, ComObject, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const SIGNATURE_IIDS: [Iid; 4] = [
    guid(
        0xc36a797c,
        0xec80,
        0x4f0a,
        [0x89, 0x85, 0xa7, 0xb2, 0x47, 0x50, 0x82, 0xd1],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Desc {
    stride: u32,
    count: u32,
    arguments: *const [u32; 4],
    nodes: u32,
}

impl Desc {
    fn arguments(&self) -> Option<Vec<[u32; 4]>> {
        match (self.count, self.arguments.is_null()) {
            (0, _) => Some(Vec::new()),
            (_, true) => None,
            (count, false) => {
                Some(unsafe { std::slice::from_raw_parts(self.arguments, count as usize) }.to_vec())
            }
        }
    }
}

#[repr(C)]
pub(crate) struct CommandSignature {
    object: Object,
    stride: u32,
    arguments: Vec<[u32; 4]>,
}

unsafe impl ComObject for CommandSignature {
    const KIND: usize = COMMAND_SIGNATURE;
    const INTERFACES: &'static [Iid] = &SIGNATURE_IIDS;
}

impl CommandSignature {
    fn new(stride: u32, arguments: Vec<[u32; 4]>) -> Box<Self> {
        Box::new(Self {
            object: Object::new(&FUNCTIONS),
            stride,
            arguments,
        })
    }

    pub(crate) fn layout<'a>(this: This) -> Option<(u32, &'a [[u32; 4]])> {
        if this.is_null() {
            return None;
        }
        let signature = Self::from_this(this);
        Some((signature.stride, &signature.arguments))
    }

    pub(crate) extern "system" fn create(
        _: This,
        desc: *const Desc,
        _root: This,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        let Some(desc) = (unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        let Some(arguments) = desc.arguments() else {
            return E_INVALIDARG;
        };
        tracing::debug!(stride = desc.stride, ?arguments, "command signature");
        Self::new(desc.stride, arguments).hand_out(iid, out)
    }
}

static FUNCTIONS: Table<24> = Table(object::pageable_table::<
    CommandSignature,
    COMMAND_SIGNATURE,
    24,
>());
