//! The raw Darwin environment for signalling. The presentation produced by
//! `ps -E` joins arguments and environment values with spaces.

#[cfg(target_os = "macos")]
pub(super) fn has_marker(pid: u32, marker: &[u8]) -> bool {
    use std::mem::size_of;
    use std::ptr::null_mut;

    let mut argmax = 0i32;
    let mut size = size_of::<i32>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: both buffers are allocated for the complete sysctl call.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut argmax as *mut i32).cast(),
            &mut size,
            null_mut(),
            0,
        )
    } != 0
        || argmax <= 0
    {
        return false;
    }
    let mut arguments = vec![0; argmax as usize];
    let mut size = arguments.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    // SAFETY: arguments has room for size bytes and sysctl reports bytes written.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            arguments.as_mut_ptr().cast(),
            &mut size,
            null_mut(),
            0,
        )
    } != 0
        || size > arguments.len()
    {
        return false;
    }
    contains_marker(&arguments[..size], marker)
}

fn contains_marker(arguments: &[u8], marker: &[u8]) -> bool {
    // Darwin returns argc, exec_path with NUL padding, argv, then envp.
    // See Apple's adv_cmds/ps/print.c, getproclline.
    let Some((argc, strings)) = arguments.split_first_chunk::<4>() else {
        return false;
    };
    let Ok(argc) = usize::try_from(i32::from_ne_bytes(*argc)) else {
        return false;
    };
    let Some(end_of_path) = strings.iter().position(|byte| *byte == 0) else {
        return false;
    };
    let padded_args = &strings[end_of_path..];
    let Some(start) = padded_args.iter().position(|byte| *byte != 0) else {
        return false;
    };
    argc > 0
        && padded_args[start..]
            .split(|byte| *byte == 0)
            .skip(argc)
            .take_while(|entry| !entry.is_empty())
            .any(|entry| entry == marker)
}

#[cfg(test)]
mod tests {
    #[test]
    fn darwin_suspend_marker_is_an_exact_environment_record() {
        let marker = b"MJ_SESSION_ID=a";
        let mut arguments = 2i32.to_ne_bytes().to_vec();
        arguments
            .extend_from_slice(b"/worker\0\0worker\0MJ_SESSION_ID=a\0TEXT=foo MJ_SESSION_ID=a\0\0");
        assert!(!super::contains_marker(&arguments, marker));

        arguments.pop();
        arguments.extend_from_slice(b"MJ_SESSION_ID=a\0\0");
        assert!(super::contains_marker(&arguments, marker));
    }
}
