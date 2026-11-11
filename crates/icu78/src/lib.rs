//! Rust-owned immutable collation over pinned ICU4C78.2 UTF16 inputs.
use std::{
    cmp::Ordering,
    ffi::{c_char, c_void, CStr, CString},
    ptr::NonNull,
    sync::{Mutex, OnceLock},
};
extern "C" {
    #[link_name = "ucol_open_78_mnemosyne"]
    fn open(locale: *const c_char, status: *mut i32) -> *mut c_void;
    #[link_name = "ucol_close_78_mnemosyne"]
    fn close(collator: *mut c_void);
    #[link_name = "ucol_setAttribute_78_mnemosyne"]
    fn set_attribute(collator: *mut c_void, attribute: i32, value: i32, status: *mut i32);
    #[link_name = "ucol_strcoll_78_mnemosyne"]
    fn compare(
        collator: *const c_void,
        left: *const u16,
        left_len: i32,
        right: *const u16,
        right_len: i32,
    ) -> i32;
    #[link_name = "uloc_forLanguageTag_78_mnemosyne"]
    fn locale_for_tag(
        tag: *const c_char,
        locale: *mut c_char,
        capacity: i32,
        parsed: *mut i32,
        status: *mut i32,
    ) -> i32;
    #[link_name = "u_strToLower_78_mnemosyne"]
    fn str_to_lower(
        dest: *mut u16,
        capacity: i32,
        source: *const u16,
        length: i32,
        locale: *const c_char,
        status: *mut i32,
    ) -> i32;
    #[link_name = "u_getVersion_78_mnemosyne"]
    fn get_version(version: *mut u8);
    #[link_name = "u_errorName_78_mnemosyne"]
    fn error_name(status: i32) -> *const c_char;
    #[link_name = "udata_setFileAccess_78_mnemosyne"]
    fn set_file_access(access: i32, status: *mut i32);
    #[link_name = "udata_setCommonData_78_mnemosyne"]
    fn set_data(data: *const c_void, status: *mut i32);

}
#[repr(C, align(16))]
struct AlignedData<const N: usize>([u8; N]);
static DATA: AlignedData<{ include_bytes!(concat!(env!("OUT_DIR"), "/icudtl.dat")).len() }> =
    AlignedData(*include_bytes!(concat!(env!("OUT_DIR"), "/icudtl.dat")));
#[derive(Debug)]
struct Owned(NonNull<c_void>);
// ICU collators are not thread-bound. Mutex serializes all access, including
// any internal caches; ownership guarantees close occurs after the final use.
unsafe impl Send for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { close(self.0.as_ptr()) }
    }
}
#[derive(Debug)]
pub struct Collator(Mutex<Owned>);
#[derive(Debug)]
pub struct Error(pub String);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
fn check(status: i32) -> Result<(), Error> {
    if status > 0 {
        Err(Error(
            unsafe { CStr::from_ptr(error_name(status)) }
                .to_string_lossy()
                .into_owned(),
        ))
    } else {
        Ok(())
    }
}
pub fn version() -> [u8; 4] {
    let mut version = [0; 4];
    unsafe { get_version(version.as_mut_ptr()) }
    version
}
fn initialize() -> Result<(), Error> {
    if version() != [78, 2, 0, 0] {
        return Err(Error(format!("Expected ICU78.2, linked {:?}", version())));
    }
    static DATA_READY: OnceLock<Result<(), String>> = OnceLock::new();
    DATA_READY
        .get_or_init(|| {
            let mut status = 0;
            unsafe {
                set_file_access(3, &mut status);
                set_data(std::ptr::addr_of!(DATA).cast(), &mut status)
            };
            check(status).map_err(|error| error.0)
        })
        .as_ref()
        .map_err(|error| Error(error.clone()))?;
    Ok(())
}
/// ECMAScript default (root-locale) lowercase over lossless UTF16 strings.
pub fn lowercase_units(input: &[u16]) -> Result<Vec<u16>, Error> {
    initialize()?;
    let length = i32::try_from(input.len())
        .map_err(|_| Error("ICU input exceeds signed32-bit length".into()))?;
    let mut status = 0;
    let needed = unsafe {
        str_to_lower(
            std::ptr::null_mut(),
            0,
            input.as_ptr(),
            length,
            c"".as_ptr(),
            &mut status,
        )
    };
    if status != 15 {
        check(status)?;
    }
    if needed < 0 {
        return Err(Error("ICU returned invalid lowercase length".into()));
    }
    let mut output = vec![0u16; needed as usize];
    status = 0;
    let actual = unsafe {
        str_to_lower(
            output.as_mut_ptr(),
            needed,
            input.as_ptr(),
            length,
            c"".as_ptr(),
            &mut status,
        )
    };
    check(status)?;
    if actual != needed {
        return Err(Error("ICU lowercase preflight length changed".into()));
    }
    Ok(output)
}
impl Collator {
    pub fn new(language_tag: &str) -> Result<Self, Error> {
        initialize()?;
        let tag = CString::new(language_tag).map_err(|_| Error("Invalid language tag".into()))?;
        let mut parsed = 0;
        let mut status = 0;
        let needed = unsafe {
            locale_for_tag(
                tag.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut parsed,
                &mut status,
            )
        };
        if status != 15 {
            check(status)?;
        }
        if parsed as usize != language_tag.len() || parsed == 0 {
            return Err(Error("Invalid language tag".into()));
        }
        let capacity = needed
            .checked_add(1)
            .filter(|capacity| *capacity > 0)
            .ok_or_else(|| Error("Invalid locale id length".into()))?;
        let mut locale = vec![0i8; capacity as usize];
        status = 0;
        unsafe {
            locale_for_tag(
                tag.as_ptr(),
                locale.as_mut_ptr(),
                capacity,
                &mut parsed,
                &mut status,
            )
        };
        check(status)?;
        status = 0;
        let pointer = unsafe { open(locale.as_ptr(), &mut status) };
        check(status)?;
        let owned =
            Owned(NonNull::new(pointer).ok_or_else(|| Error("ICU collator returned null".into()))?);
        // Pinned ucol.h constants: numeric7=ON17, strength5=PRIMARY0,
        // case-level3=OFF16, normalization4=ON17. Alternate handling stays
        // locale-defined (e.g. Thai), matching Intl's omitted ignorePunctuation.
        for (attribute, value) in [(7, 17), (5, 0), (3, 16), (4, 17)] {
            status = 0;
            unsafe { set_attribute(owned.0.as_ptr(), attribute, value, &mut status) };
            check(status)?
        }
        Ok(Self(Mutex::new(owned)))
    }
    pub fn compare(&self, left: &[u16], right: &[u16]) -> Ordering {
        let left_len = i32::try_from(left.len()).expect("ICU input exceeds signed32-bit length");
        let right_len = i32::try_from(right.len()).expect("ICU input exceeds signed32-bit length");
        let owned = self.0.lock().unwrap();
        unsafe {
            compare(
                owned.0.as_ptr(),
                left.as_ptr(),
                left_len,
                right.as_ptr(),
                right_len,
            )
        }
        .cmp(&0)
    }
}
