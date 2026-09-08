/// Prepare only this process; children inherit the resulting Unix soft limit.
pub fn prepare() {
    #[cfg(unix)]
    match raise_nofile() {
        Ok(actual) if actual < TARGET_NOFILE => eprintln!(
            "warning: file descriptor soft limit is {actual}; requested {TARGET_NOFILE}, limited by the hard limit"
        ),
        Err(error) => eprintln!(
            "warning: unable to raise file descriptor soft limit to {TARGET_NOFILE}: {error}"
        ),
        _ => {}
    }
}

#[cfg(unix)]
const TARGET_NOFILE: libc::rlim_t = 65_536;

#[cfg(unix)]
fn desired_soft_limit(soft: libc::rlim_t, hard: libc::rlim_t) -> libc::rlim_t {
    soft.max(TARGET_NOFILE.min(hard))
}

#[cfg(unix)]
fn raise_nofile() -> std::io::Result<libc::rlim_t> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // Both calls operate on this process, with a valid rlimit pointer; the hard limit is unchanged.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let desired = desired_soft_limit(limit.rlim_cur, limit.rlim_max);
    if desired != limit.rlim_cur {
        limit.rlim_cur = desired;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(limit.rlim_cur)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn nofile_target_respects_existing_soft_and_hard_limits() {
        assert_eq!(desired_soft_limit(256, libc::RLIM_INFINITY), 65_536);
        assert_eq!(desired_soft_limit(256, 4096), 4096);
        assert_eq!(desired_soft_limit(131_072, libc::RLIM_INFINITY), 131_072);
        assert_eq!(
            desired_soft_limit(libc::RLIM_INFINITY, libc::RLIM_INFINITY),
            libc::RLIM_INFINITY
        );
    }
}
