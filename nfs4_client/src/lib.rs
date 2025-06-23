// Copyright 2023 Remi Bernotavicius

use derive_more::From;
use nfs4::*;
use paste::paste;
use rand::Rng as _;
use std::collections::VecDeque;
use std::io;
use std::path::{Component, Path};
use sun_rpc_client::{RpcClient, Transport};
pub use sun_rpc_client::{AuthSysParameters, Gid, OpaqueAuth, Uid};

pub type Result<T> = std::result::Result<T, Error>;

pub struct TempResult<T>(Result<T>);

impl<T> From<StatusResult<T>> for TempResult<T> {
    fn from(res: StatusResult<T>) -> Self {
        TempResult(match res {
            StatusResult::Ok(res) => Ok(res),
            StatusResult::Err(e) => Err(e.into()),
        })
    }
}

impl From<SetAttrStatusResult> for TempResult<SetAttrRes> {
    fn from(res: SetAttrStatusResult) -> Self {
        TempResult(match res.status {
            StatusResult::Ok(_) => Ok(res.res),
            StatusResult::Err(e) => Err(e.into()),
        })
    }
}

impl<T> From<LockStatusResult<T>> for TempResult<T> {
    fn from(res: LockStatusResult<T>) -> Self {
        TempResult(match res {
            LockStatusResult::Ok(res) => Ok(res),
            LockStatusResult::Err(e) => Err(e.into()),
        })
    }
}

#[derive(Debug, From)]
pub enum Error {
    SunRpc(sun_rpc_client::Error),
    Protocol(StatusError),
    Lock(LockStatusError),
    Io(std::io::Error),
    #[from(ignore)]
    CompoundResponseMismatch(String),
}

const NFS: u32 = 100003;
const NFS_CB: u32 = 0x40000000;
pub const NFS_PORT: u16 = 2049;
const COMPOUND_PROCEDURE: u32 = 1;

macro_rules! compound_op_impl_ {
    ($name:ident, $args:ident, $res:ty) => {
        impl CompoundRequest for $args {
            type Response = $res;
            type Geometry = ();

            fn into_arg_array(self) -> (Vec<ArgOp>, Self::Geometry) {
                (vec![self.into()], ())
            }

            fn process_reply(
                res_array: &mut VecDeque<ResOp>,
                _geometry: (),
            ) -> Result<Self::Response> {
                let op = res_array
                    .pop_front()
                    .ok_or(Error::CompoundResponseMismatch("too few replies".into()))?;

                if let ResOp::$name(res) = op {
                    TempResult::from(res).0
                } else {
                    Err(Error::CompoundResponseMismatch(format!(
                        "expected {}, got {op:?}",
                        stringify!($name)
                    )))
                }
            }
        }
    };
}

macro_rules! compound_op_impl {
    ($($name:ident)*) => ($(paste! {
        compound_op_impl_! { $name, [<$name Args>], [<$name Res>] }
    })*)
}

macro_rules! compound_op_impl_no_args {
    ($($name:ident)*) => ($(paste! {
        #[derive(Copy, Clone)]
        pub struct $name;

        impl From<$name> for ArgOp {
            fn from(_: $name) -> ArgOp {
                ArgOp::$name
            }
        }

        compound_op_impl_! { $name, $name, [<$name Res>] }
    })*)
}

macro_rules! compound_op_impl_no_args_no_ret {
    ($($name:ident)*) => ($(paste! {
        #[derive(Copy, Clone)]
        pub struct $name;

        impl From<$name> for ArgOp {
            fn from(_: $name) -> ArgOp {
                ArgOp::$name
            }
        }

        compound_op_impl_! { $name, $name, () }
    })*)
}

macro_rules! compound_op_impl_no_ret {
    ($($name:ident)*) => ($(paste! {
        compound_op_impl_! { $name, [<$name Args>], () }
    })*)
}

compound_op_impl! {
    Access
    Close
    Commit
    Create
    GetAttr
    Link
    Lock
    LockU
    Open
    OpenDowngrade
    Read
    ReadDir
    Remove
    Rename
    SecInfo
    SetAttr
    Write
    BindConnToSession
    ExchangeId
    CreateSession
    GetDirDelegation
    GetDeviceInfo
    GetDeviceList
    LayoutCommit
    LayoutGet
    LayoutReturn
    SecInfoNoName
    Sequence
    SetSsv
    TestStateId
    WantDelegation
}

compound_op_impl_no_ret! {
    DelegPurge
    DelegReturn
    LockT
    LookUp
    NVerify
    OpenAttr
    PutFh
    Verify
    BackchannelCtl
    DestroySession
    FreeStateid
    DestroyClientId
    ReclaimComplete
}

compound_op_impl_no_args! {
    GetFh
    ReadLink
}

compound_op_impl_no_args_no_ret! {
    PutPubFh
    LookUpP
    PutRootFh
    RestoreFh
    SaveFh
}

trait CompoundRequest {
    type Response;
    type Geometry;

    fn into_arg_array(self) -> (Vec<ArgOp>, Self::Geometry);

    fn process_reply(
        res_array: &mut VecDeque<ResOp>,
        geometry: Self::Geometry,
    ) -> Result<Self::Response>;
}

impl<T> CompoundRequest for Vec<T>
where
    T: CompoundRequest,
{
    type Response = Vec<<T as CompoundRequest>::Response>;
    type Geometry = Vec<<T as CompoundRequest>::Geometry>;

    fn into_arg_array(self) -> (Vec<ArgOp>, Self::Geometry) {
        let mut arg_ret = vec![];
        let mut geo_ret = vec![];
        for e in self {
            let (v, g) = e.into_arg_array();
            arg_ret.extend(v);
            geo_ret.push(g);
        }
        (arg_ret, geo_ret)
    }

    fn process_reply(
        res_array: &mut VecDeque<ResOp>,
        geometry: Self::Geometry,
    ) -> Result<Self::Response> {
        let mut ret = vec![];
        for geo in geometry {
            ret.push(T::process_reply(res_array, geo)?);
        }
        Ok(ret)
    }
}

struct ReturnSecond<A, B>(A, B);

impl<A, B> CompoundRequest for ReturnSecond<A, B>
where
    A: CompoundRequest,
    B: CompoundRequest,
{
    type Response = B::Response;
    type Geometry = (A::Geometry, B::Geometry);

    fn into_arg_array(self) -> (Vec<ArgOp>, Self::Geometry) {
        let (mut a1, g1) = self.0.into_arg_array();
        let (a2, g2) = self.1.into_arg_array();
        a1.extend(a2);
        (a1, (g1, g2))
    }

    fn process_reply(
        res_array: &mut VecDeque<ResOp>,
        geometry: Self::Geometry,
    ) -> Result<Self::Response> {
        A::process_reply(res_array, geometry.0)?;
        B::process_reply(res_array, geometry.1)
    }
}

macro_rules! tuple_impls {
    ($(($($n:tt $name:ident)+))+) => {$(paste! {
        impl<$($name),+> CompoundRequest for ($($name),+)
        where
            $($name: CompoundRequest,)+
        {
            type Response = ($(<$name as CompoundRequest>::Response),+);
            type Geometry = ($(<$name as CompoundRequest>::Geometry),+);

            fn into_arg_array(self) -> (Vec<ArgOp>, Self::Geometry) {
                $(let ([<v $n>], [<g $n>]) = self.$n.into_arg_array();)+

                let mut v = vec![];
                $(v.extend([<v $n>]);)+
                (
                    v,
                    ($([<g $n>]),+)
                )
            }

            fn process_reply(
                res_array: &mut VecDeque<ResOp>, geometry: Self::Geometry
            ) -> Result<Self::Response> {
                Ok((
                    $(
                        $name::process_reply(res_array, geometry.$n)?
                    ),+
                ))
            }
        }
    })+}
}

tuple_impls! {
    (0 A 1 B)
    (0 A 1 B 2 C)
    (0 A 1 B 2 C 3 D)
    (0 A 1 B 2 C 3 D 4 E)
    (0 A 1 B 2 C 3 D 4 E 5 F)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J 10 K)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J 10 K 11 L)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J 10 K 11 L 12 M)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J 10 K 11 L 12 M 13 N)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J 10 K 11 L 12 M 13 N 14 O)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J 10 K 11 L 12 M 13 N 14 O 15 P)
    (0 A 1 B 2 C 3 D 4 E 5 F 6 G 7 H 8 I 9 J 10 K 11 L 12 M 13 N 14 O 15 P 16 Q)
}

struct ClientWithoutSession<TransportT> {
    rpc_client: RpcClient<TransportT>,
}

impl<TransportT: Transport> ClientWithoutSession<TransportT> {
    fn new(rpc_client: RpcClient<TransportT>) -> Self {
        Self { rpc_client }
    }

    fn do_null(&mut self) -> Result<()> {
        self.rpc_client.send_request(sun_rpc_client::NULL_PROCEDURE, ())?;
        Ok(self.rpc_client.receive_reply::<()>()?)
    }

    fn do_compound<Args>(&mut self, args: Args) -> Result<Args::Response>
    where
        Args: CompoundRequest,
    {
        let (arg_array, geometry) = args.into_arg_array();
        let call_args = CompoundArgs {
            tag: "Test Client".into(),
            minor_version: 1,
            arg_array,
        };

        self.rpc_client
            .send_request(COMPOUND_PROCEDURE, call_args)?;

        let compound_reply: CompoundRes = self.rpc_client.receive_reply()?;

        if let StatusResult::Err(e) = compound_reply.status {
            return Err(e.into());
        }

        let mut res_array = compound_reply.res_array.into_iter().collect();
        let reply = Args::process_reply(&mut res_array, geometry)?;

        if !res_array.is_empty() {
            return Err(Error::CompoundResponseMismatch(format!(
                "trailing response: {res_array:?}"
            )));
        }

        Ok(reply)
    }
}

fn random_client_owner() -> ClientOwner {
    let mut rng = rand::thread_rng();
    ClientOwner {
        verifier: Verifier(0x0),
        owner_id: rng.gen::<u64>().to_be_bytes().into(),
    }
}

pub struct Client<TransportT> {
    raw_client: ClientWithoutSession<TransportT>,
    session: CreateSessionRes,
    sequence_id: SequenceId,
    client_id: ClientId,
    client_owner: ClientOwner,
    max_read: u64,
    max_write: u64,
    supported_attrs: EnumSet<FileAttributeId>,
}

impl<TransportT: Transport> Client<TransportT> {
    pub fn new(transport: TransportT, credential: Option<OpaqueAuth>) -> Result<Self> {
        let mut raw_client = ClientWithoutSession::new(RpcClient::new(transport, NFS, credential));

        let client_owner = random_client_owner();
        let eid_res = raw_client.do_compound(ExchangeIdArgs {
            client_owner: client_owner.clone(),
            flags: ExchangeIdFlags::empty(),
            state_protect: StateProtect::None,
            client_impl_id: None,
        })?;

        let client_id = eid_res.client_id;
        let session = raw_client.do_compound(CreateSessionArgs {
            client_id,
            sequence_id: SequenceId(1),
            flags: CreateSessionFlags::empty(),
            fore_channel_attrs: ChannelAttrs {
                header_pad_size: 0,
                max_request_size: 1049620,
                max_response_size: 1049480,
                max_response_size_cached: 7584,
                max_operations: 16,
                max_requests: 64,
                rdma_ird: None,
            },
            back_channel_attrs: ChannelAttrs {
                header_pad_size: 0,
                max_request_size: 4096,
                max_response_size: 4096,
                max_response_size_cached: 0,
                max_operations: 16,
                max_requests: 16,
                rdma_ird: None,
            },
            program: NFS_CB,
            security_parameters: vec![],
        })?;

        let mut client = Self {
            raw_client,
            session,
            sequence_id: SequenceId(1),
            client_id,
            client_owner,
            max_read: 0,
            max_write: 0,
            supported_attrs: Default::default(),
        };

        let mut root_attrs = client
            .do_compound(ReturnSecond(
                (ReclaimCompleteArgs { one_fs: false }, PutRootFh),
                GetAttrArgs {
                    attr_request: [
                        FileAttributeId::SupportedAttrs,
                        FileAttributeId::MaxRead,
                        FileAttributeId::MaxWrite,
                    ]
                    .into_iter()
                    .collect(),
                },
            ))?
            .object_attributes;

        client.supported_attrs = root_attrs
            .remove_as(FileAttributeId::SupportedAttrs)
            .unwrap();
        client.max_read = *root_attrs.get_as(FileAttributeId::MaxRead).unwrap();
        client.max_write = *root_attrs.get_as(FileAttributeId::MaxWrite).unwrap();

        Ok(client)
    }

    fn do_compound<Args>(&mut self, args: Args) -> Result<Args::Response>
    where
        Args: CompoundRequest,
    {
        let sequence = SequenceArgs {
            session_id: self.session.session_id,
            sequence_id: self.sequence_id,
            slot_id: SlotId(0),
            highest_slot_id: SlotId(0),
            cache_this: false,
        };

        self.sequence_id.incr();

        self.raw_client.do_compound(ReturnSecond(sequence, args))
    }

    pub fn null(&mut self) -> Result<()> {
        self.raw_client.do_null()
    }

    pub fn access(&mut self, handle: FileHandle, access: u32) -> Result<AccessRes> {
        self.do_compound(ReturnSecond(
            PutFhArgs { object: handle },
            AccessArgs {
                access: Access::from_bits_truncate(access),
            },
        ))
    }

    pub fn commit(&mut self, handle: FileHandle, offset: u64, count: u32) -> Result<CommitRes> {
        self.do_compound(ReturnSecond(
            PutFhArgs { object: handle },
            CommitArgs {
                offset,
                count,
            },
        ))
    }

    pub fn get_attr(&mut self, handle: FileHandle) -> Result<GetAttrRes> {
        let mut supported_attrs = self.supported_attrs.clone();

        supported_attrs.remove(FileAttributeId::TimeAccessSet);
        supported_attrs.remove(FileAttributeId::TimeModifySet);

        self.do_compound(ReturnSecond(
            PutFhArgs { object: handle },
            GetAttrArgs {
                attr_request: supported_attrs,
            },
        ))
    }

    pub fn look_up_in(&mut self, dir_handle: FileHandle, path: impl AsRef<Path>) -> Result<FileHandle> {
        self.do_compound(ReturnSecond(
                (
                    PutFhArgs { object: dir_handle },
                    LookUpArgs { object_name: path.as_ref().as_os_str().to_str().unwrap().into() },
                ),
                GetFh,
            ))
            .map(|res| res.object)
    }

    pub fn look_up(&mut self, path: impl AsRef<Path>) -> Result<FileHandle> {
        self.do_compound(ReturnSecond(
                (
                    PutRootFh,
                    Vec::from_iter(path.as_ref().components().filter_map(|c| match c {
                        Component::Normal(p) => Some(LookUpArgs {
                            object_name: p.to_str().unwrap().into(),
                        }),
                        _ => None,
                    })),
                ),
                GetFh,
            ))
            .map(|res| res.object)
    }

    pub fn read(&mut self, handle: FileHandle, offset: u64, count: u32) -> Result<ReadRes> {
        self.do_compound(ReturnSecond(
            PutFhArgs { object: handle },
            ReadArgs {
                state_id: StateId::anonymous(),
                offset,
                count,
            },
        ))
    }

    pub fn read_all(&mut self, handle: FileHandle, mut sink: impl io::Write) -> Result<()> {
        let mut offset = 0;
        loop {
            let read_res = self.read(handle.clone(), offset, self.max_read.try_into().unwrap())?;
            offset += read_res.data.len() as u64;
            sink.write_all(&read_res.data)?;
            if read_res.eof {
                break;
            }
        }
        Ok(())
    }

    pub fn write(&mut self, handle: FileHandle, offset: u64, data: Vec<u8>) -> Result<WriteRes> {
        self.do_compound(ReturnSecond(
            PutFhArgs { object: handle },
            WriteArgs {
                state_id: StateId::anonymous(),
                offset,
                stable: StableHow::FileSync,
                data,
            },
        ))
    }

    pub fn write_all(&mut self, handle: FileHandle, mut source: impl io::Read) -> Result<()> {
        let mut offset = 0;
        loop {
            let mut buf = vec![0; self.max_write as usize];
            let amount_read = source.read(&mut buf[..])?;
            if amount_read == 0 {
                break;
            }

            buf.resize(amount_read, 0);

            while !buf.is_empty() {
                let write_res = self.write(handle.clone(), offset, buf.clone())?;
                buf = buf[write_res.count as usize..].to_owned();
            }

            offset += amount_read as u64;
        }
        Ok(())
    }

    pub fn create_file(
        &mut self,
        parent: FileHandle,
        name: &str,
        attrs: FileAttributes,
    ) -> Result<FileHandle> {
        self.do_compound(ReturnSecond(
                (
                    PutFhArgs { object: parent },
                    OpenArgs {
                        sequence_id: SequenceId(0),
                        share_access: ShareAccess::WRITE,
                        share_deny: ShareDeny::NONE,
                        owner: StateOwner {
                            client_id: self.client_id,
                            opaque: self.client_owner.owner_id.clone(),
                        },
                        open_how: OpenFlag::OpenCreate(CreateHow::Guarded {
                            create_attrs: attrs,
                        }),
                        claim: OpenClaim::Null { file: name.into() },
                    },
                ),
                GetFh,
            ))
            .map(|res| res.object)
    }

    pub fn read_dir(
        &mut self,
        handle: FileHandle,
        attr_request: EnumSet<FileAttributeId>,
    ) -> Result<Vec<DirectoryEntry>> {
        let mut entries = vec![];
        let attr_request: EnumSet<_> = attr_request
            .into_iter()
            .filter(|a| self.supported_attrs.contains(*a))
            .collect();

        let mut cookie = Cookie::initial();
        loop {
            let res = self.do_compound(ReturnSecond(
                PutFhArgs {
                    object: handle.clone(),
                },
                ReadDirArgs {
                    cookie,
                    cookie_verifier: Verifier(0),
                    directory_count: 1000,
                    max_count: 1000,
                    attr_request: attr_request.clone(),
                },
            ))?;

            entries.extend(res.reply.entries);

            if res.reply.eof {
                break Ok(entries);
            }
            cookie = entries.last().unwrap().cookie;
        }
    }

    pub fn set_attr(&mut self, handle: FileHandle, attrs: FileAttributes) -> Result<()> {
        self.do_compound(ReturnSecond(
            PutFhArgs { object: handle },
            SetAttrArgs {
                state_id: StateId::anonymous(),
                object_attributes: attrs,
            },
        ))?;
        Ok(())
    }

    pub fn set_attr_verified(&mut self, handle: FileHandle, attrs: FileAttributes, verif_attrs: FileAttributes) -> Result<()> {
        self.do_compound(ReturnSecond(
            (
                PutFhArgs { object: handle },
                VerifyArgs { object_attributes: verif_attrs },
            ),
            SetAttrArgs {
                state_id: StateId::anonymous(),
                object_attributes: attrs,
            },
        ))?;
        Ok(())
    }

    pub fn remove(&mut self, handle: FileHandle, entry_name: &str) -> Result<ChangeInfo> {
        self.do_compound(ReturnSecond(
                PutFhArgs { object: handle },
                RemoveArgs {
                    target: entry_name.into(),
                },
            ))
            .map(|res| res.change_info)
    }

    pub fn rename(
        &mut self,
        src_dir: FileHandle,
        target_dir: FileHandle,
        src_entry: &str,
        target_entry: &str,
    ) -> Result<RenameRes> {
        self.do_compound(ReturnSecond(
            (
                PutFhArgs { object: src_dir },
                SaveFh,
                PutFhArgs { object: target_dir },
            ),
            RenameArgs {
                old_name: src_entry.to_owned(),
                new_name: target_entry.to_owned(),
            },
        ))
    }

    pub fn create_directory(
        &mut self,
        parent_dir: FileHandle,
        name: &str,
        attrs: FileAttributes,
    ) -> Result<FileHandle> {
        self.do_compound(ReturnSecond(
                (
                    PutFhArgs { object: parent_dir },
                    CreateArgs {
                        object_type: CreateType::Directory,
                        object_name: name.to_owned(),
                        create_attrs: attrs,
                    },
                ),
                GetFh,
            ))
            .map(|res| res.object)
    }

    pub fn create_link(
        &mut self,
        src_path: &str,
        parent_dir: FileHandle,
        name: &str,
        attrs: FileAttributes,
    ) -> Result<FileHandle> {
        self.do_compound(ReturnSecond(
                (
                    PutFhArgs { object: parent_dir },
                    CreateArgs {
                        object_type: CreateType::Link(src_path.to_string()),
                        object_name: name.to_owned(),
                        create_attrs: attrs,
                    },
                ),
                GetFh,
            ))
            .map(|res| res.object)
    }

    pub fn read_link(&mut self, handle: FileHandle) -> Result<ReadLinkRes> {
        self.do_compound(ReturnSecond(
            PutFhArgs { object: handle },
            ReadLink { },
        ))
    }

    pub fn get_max_read_size(&self) -> u64 {
        self.max_read
    }

    pub fn get_max_write_size(&self) -> u64 {
        self.max_write
    }
}
