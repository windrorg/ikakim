//! AVFoundation カメラデバイス列挙・キャプチャブリッジ（macOS / iOS 共通）

use std::ffi::{c_char, c_void, CStr};

type Id = *mut c_void;
type Sel = *mut c_void;
type Class = *const c_void;

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Class;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

/// 検出されたビデオキャプチャデバイス情報
#[derive(Debug, Clone)]
pub struct AvCaptureDeviceInfo {
    pub unique_id: String,
    pub localized_name: String,
}

/// AVFoundation (`AVCaptureDevice`) を用いて利用可能なカメラデバイス一覧を取得する
pub fn list_video_devices() -> Vec<AvCaptureDeviceInfo> {
    let mut devices = Vec::new();
    unsafe {
        let cls = objc_getClass(c"AVCaptureDevice".as_ptr());
        if cls.is_null() {
            return devices;
        }
        // AVMediaTypeVideo = @"vide"
        let nsstring_cls = objc_getClass(c"NSString".as_ptr());
        let sel_str = sel_registerName(c"stringWithUTF8String:".as_ptr());
        let msg_str: unsafe extern "C" fn(Class, Sel, *const c_char) -> Id =
            std::mem::transmute(objc_msgSend as *const c_void);
        let media_type = msg_str(nsstring_cls, sel_str, c"vide".as_ptr());

        let sel_devices = sel_registerName(c"devicesWithMediaType:".as_ptr());
        let msg_devices: unsafe extern "C" fn(Class, Sel, Id) -> Id =
            std::mem::transmute(objc_msgSend as *const c_void);
        let ns_array = msg_devices(cls, sel_devices, media_type);
        if ns_array.is_null() {
            return devices;
        }

        let sel_count = sel_registerName(c"count".as_ptr());
        let sel_obj = sel_registerName(c"objectAtIndex:".as_ptr());
        let sel_uid = sel_registerName(c"uniqueID".as_ptr());
        let sel_name = sel_registerName(c"localizedName".as_ptr());
        let sel_utf8 = sel_registerName(c"UTF8String".as_ptr());

        let msg_count: unsafe extern "C" fn(Id, Sel) -> usize =
            std::mem::transmute(objc_msgSend as *const c_void);
        let msg_idx: unsafe extern "C" fn(Id, Sel, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const c_void);
        let msg_id: unsafe extern "C" fn(Id, Sel) -> Id =
            std::mem::transmute(objc_msgSend as *const c_void);
        let msg_cstr: unsafe extern "C" fn(Id, Sel) -> *const c_char =
            std::mem::transmute(objc_msgSend as *const c_void);

        let count = msg_count(ns_array, sel_count);
        for i in 0..count {
            let dev = msg_idx(ns_array, sel_obj, i);
            if dev.is_null() {
                continue;
            }
            let uid_ns = msg_id(dev, sel_uid);
            let name_ns = msg_id(dev, sel_name);
            let uid = if uid_ns.is_null() {
                String::new()
            } else {
                let p = msg_cstr(uid_ns, sel_utf8);
                if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            let name = if name_ns.is_null() {
                String::new()
            } else {
                let p = msg_cstr(name_ns, sel_utf8);
                if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            devices.push(AvCaptureDeviceInfo {
                unique_id: uid,
                localized_name: name,
            });
        }
    }
    devices
}
