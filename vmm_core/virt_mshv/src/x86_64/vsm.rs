use anyhow::Context;
use headervec::HeaderVec;
use hvdef::HvPartitionPrivilege;
use hvdef::HvPartitionPropertyCode;
use hvdef::HvRegisterValue;
use hvdef::HvRegisterVsmPartitionStatus;
use hvdef::HvRegisterVsmVpStatus;
use hvdef::HvX64RegisterName;
use hvdef::HypercallCode;
use hvdef::Vtl;
use hvdef::hypercall::EnablePartitionVtl;
use hvdef::hypercall::EnablePartitionVtlFlags;
use hvdef::hypercall::EnableVpVtlX64;
use hvdef::hypercall::GetSetVpRegisters;
use hvdef::hypercall::InitialVpContextX64;
use mshv_bindings::mshv_root_hvcall;
use mshv_ioctls::Mshv;
use mshv_ioctls::VmFd;
use zerocopy::FromZeros;

/// Native VSM state observed on a disposable child partition.
#[derive(Debug)]
pub struct VsmProbeResult {
    /// VTLs enabled at partition scope.
    pub enabled_vtls: u16,
    /// Maximum VTL reported by Hyper-V.
    pub maximum_vtl: u8,
    /// VTLs enabled on the BSP.
    pub bsp_enabled_vtls: u16,
}

fn partition_vtl_input(vtl: Vtl) -> anyhow::Result<EnablePartitionVtl> {
    anyhow::ensure!(vtl != Vtl::Vtl0, "VTL0 is already enabled");
    Ok(EnablePartitionVtl {
        partition_id: 0,
        target_vtl: vtl as u8,
        flags: EnablePartitionVtlFlags::new(),
        reserved_z0: 0,
        reserved_z1: 0,
    })
}

fn enable_partition_vtl(vmfd: &VmFd, vtl: Vtl) -> anyhow::Result<()> {
    let input = partition_vtl_input(vtl)?;
    let mut args = mshv_root_hvcall {
        code: HypercallCode::HvCallEnablePartitionVtl.0,
        in_sz: size_of::<EnablePartitionVtl>() as u16,
        in_ptr: std::ptr::addr_of!(input) as u64,
        ..Default::default()
    };
    vmfd.hvcall(&mut args).map_err(crate::KernelError::from)?;
    Ok(())
}

fn vp_vtl_input(
    vp_index: u32,
    vtl: Vtl,
    context: InitialVpContextX64,
) -> anyhow::Result<EnableVpVtlX64> {
    anyhow::ensure!(vtl != Vtl::Vtl0, "VTL0 is already enabled");
    Ok(EnableVpVtlX64 {
        partition_id: 0,
        vp_index,
        target_vtl: vtl as u8,
        reserved: [0; 3],
        vp_vtl_context: context,
    })
}

fn enable_vp_vtl(
    vmfd: &VmFd,
    vp_index: u32,
    vtl: Vtl,
    context: InitialVpContextX64,
) -> anyhow::Result<()> {
    let input = vp_vtl_input(vp_index, vtl, context)?;
    let mut args = mshv_root_hvcall {
        code: HypercallCode::HvCallEnableVpVtl.0,
        in_sz: size_of::<EnableVpVtlX64>() as u16,
        in_ptr: std::ptr::addr_of!(input) as u64,
        ..Default::default()
    };
    vmfd.hvcall(&mut args).map_err(crate::KernelError::from)?;
    Ok(())
}

fn register_input(vp_index: u32, vtl: Vtl) -> GetSetVpRegisters {
    GetSetVpRegisters {
        partition_id: 0,
        vp_index,
        target_vtl: vtl.into(),
        rsvd: [0; 3],
    }
}

fn reset_context() -> InitialVpContextX64 {
    let data_segment = hvdef::HvX64SegmentRegister {
        limit: 0xffff,
        attributes: 0x93,
        ..FromZeros::new_zeroed()
    };
    InitialVpContextX64 {
        rip: 0xfff0,
        rflags: 2,
        cs: hvdef::HvX64SegmentRegister {
            base: 0xffff0000,
            limit: 0xffff,
            selector: 0xf000,
            attributes: 0x9b,
        },
        ds: data_segment,
        es: data_segment,
        fs: data_segment,
        gs: data_segment,
        ss: data_segment,
        tr: hvdef::HvX64SegmentRegister {
            limit: 0xffff,
            attributes: 0x8b,
            ..FromZeros::new_zeroed()
        },
        idtr: hvdef::HvX64TableRegister {
            limit: 0xffff,
            ..FromZeros::new_zeroed()
        },
        gdtr: hvdef::HvX64TableRegister {
            limit: 0xffff,
            ..FromZeros::new_zeroed()
        },
        cr0: 0x60000010,
        msr_cr_pat: 0x0007040600070406,
        ..FromZeros::new_zeroed()
    }
}

fn get_register(
    vmfd: &VmFd,
    vp_index: u32,
    vtl: Vtl,
    name: HvX64RegisterName,
) -> anyhow::Result<HvRegisterValue> {
    let mut input = HeaderVec::<_, u32, 1>::new(register_input(vp_index, vtl));
    input.push_tail(name.0);
    let mut output = HvRegisterValue::new_zeroed();
    let mut args = mshv_root_hvcall {
        code: HypercallCode::HvCallGetVpRegisters.0,
        reps: 1,
        in_sz: input.total_byte_len() as u16,
        out_sz: size_of::<HvRegisterValue>() as u16,
        in_ptr: input.as_ptr() as u64,
        out_ptr: std::ptr::addr_of_mut!(output) as u64,
        ..Default::default()
    };
    vmfd.hvcall(&mut args).map_err(crate::KernelError::from)?;
    anyhow::ensure!(args.reps == 1, "incomplete VSM register read");
    Ok(output)
}

/// Probes native VTL2 enablement without running guest code or changing the host VTL.
/// Requires the VSM-enabled MSHV driver and permission to open /dev/mshv.
pub fn probe_vtl2() -> anyhow::Result<VsmProbeResult> {
    let mshv = Mshv::new().context("open /dev/mshv for the VTL2 probe")?;
    let args =
        super::partition_create_args(&virt::ProtoPartitionIsolation::None, Default::default())?;
    let vmfd = crate::create_vm_with_retry(&mshv, &args)?;
    vmfd.set_partition_property(
        HvPartitionPropertyCode::SyntheticProcFeatures.0,
        u64::from(crate::common_synthetic_features()),
    )
    .context("set synthetic features for the VTL2 probe")?;
    let privileges = vmfd
        .get_partition_property(HvPartitionPropertyCode::PrivilegeFlags.0)
        .context("read child partition privileges")?;
    vmfd.set_partition_property(
        HvPartitionPropertyCode::PrivilegeFlags.0,
        HvPartitionPrivilege::from(privileges)
            .with_access_vsm(true)
            .into(),
    )
    .context("grant VSM access to the child partition")?;
    vmfd.initialize()
        .context("initialize the VTL2 probe partition")?;
    let _bsp = vmfd.create_vcpu(0).context("create the VTL2 probe BSP")?;
    enable_partition_vtl(&vmfd, Vtl::Vtl2).context("enable child partition VTL2")?;
    let status = HvRegisterVsmPartitionStatus::from(
        get_register(&vmfd, 0, Vtl::Vtl0, HvX64RegisterName::VsmPartitionStatus)
            .context("read VSM partition status")?
            .as_u64(),
    );
    anyhow::ensure!(
        status.enabled_vtl_set() & (1 << Vtl::Vtl2 as u8) != 0,
        "Hyper-V did not report VTL2 enabled"
    );
    enable_vp_vtl(&vmfd, 0, Vtl::Vtl2, reset_context()).context("enable BSP VTL2")?;
    let vp_status = HvRegisterVsmVpStatus::from(
        get_register(&vmfd, 0, Vtl::Vtl0, HvX64RegisterName::VsmVpStatus)
            .context("read BSP VSM status")?
            .as_u64(),
    );
    anyhow::ensure!(
        vp_status.enabled_vtl_set() & (1 << Vtl::Vtl2 as u8) != 0,
        "Hyper-V did not report BSP VTL2 enabled"
    );
    Ok(VsmProbeResult {
        enabled_vtls: status.enabled_vtl_set(),
        maximum_vtl: status.maximum_vtl(),
        bsp_enabled_vtls: vp_status.enabled_vtl_set(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsm_enablement_layouts_match_kernel() {
        assert_eq!(size_of::<EnablePartitionVtl>(), 16);
        assert_eq!(size_of::<InitialVpContextX64>(), 224);
        assert_eq!(size_of::<EnableVpVtlX64>(), 240);
        assert_eq!(size_of::<GetSetVpRegisters>(), 16);
    }

    #[test]
    fn vsm_enablement_rejects_vtl0() {
        assert!(partition_vtl_input(Vtl::Vtl0).is_err());
        assert!(vp_vtl_input(0, Vtl::Vtl0, FromZeros::new_zeroed()).is_err());
    }

    #[test]
    fn vsm_enablement_preserves_vtl_and_context() {
        for vtl in [Vtl::Vtl1, Vtl::Vtl2] {
            let partition = partition_vtl_input(vtl).unwrap();
            assert_eq!(partition.partition_id, 0);
            assert_eq!(partition.target_vtl, vtl as u8);
            assert_eq!(u8::from(partition.flags), 0);
            let context = InitialVpContextX64 {
                rip: 0x1000,
                rsp: 0x2000,
                rflags: 2,
                ..FromZeros::new_zeroed()
            };
            let vp = vp_vtl_input(7, vtl, context).unwrap();
            assert_eq!(vp.vp_index, 7);
            assert_eq!(vp.target_vtl, vtl as u8);
            assert_eq!(vp.reserved, [0; 3]);
            assert_eq!(vp.vp_vtl_context.rip, 0x1000);
            assert_eq!(vp.vp_vtl_context.rsp, 0x2000);
        }
    }

    #[test]
    fn vsm_register_reads_select_explicit_vtl() {
        for vtl in [Vtl::Vtl0, Vtl::Vtl1, Vtl::Vtl2] {
            let input = register_input(7, vtl);
            assert_eq!(input.partition_id, 0);
            assert_eq!(input.vp_index, 7);
            assert_eq!(input.target_vtl.target_vtl().unwrap(), Some(vtl));
            assert_eq!(input.rsvd, [0; 3]);
        }
    }

    #[test]
    fn vsm_probe_context_is_real_mode_reset() {
        let context = reset_context();
        assert_eq!(context.cs.base + context.rip, 0xfffffff0);
        assert_eq!(context.cs.selector, 0xf000);
        assert_eq!(context.cr0 & 1, 0);
        assert_eq!(context.rflags, 2);
        assert_eq!(context.efer, 0);
    }

    #[test]
    #[ignore = "requires the patched MSHV driver and access to /dev/mshv"]
    fn vsm_kernel_rejects_malformed_enablement() {
        let mshv = Mshv::new().unwrap();
        let create_args = super::super::partition_create_args(
            &virt::ProtoPartitionIsolation::None,
            Default::default(),
        )
        .unwrap();
        let vmfd = crate::create_vm_with_retry(&mshv, &create_args).unwrap();
        vmfd.initialize().unwrap();
        let _bsp = vmfd.create_vcpu(0).unwrap();

        for input in [
            EnablePartitionVtl {
                target_vtl: 0,
                ..partition_vtl_input(Vtl::Vtl2).unwrap()
            },
            EnablePartitionVtl {
                target_vtl: 3,
                ..partition_vtl_input(Vtl::Vtl2).unwrap()
            },
            EnablePartitionVtl {
                reserved_z0: 1,
                ..partition_vtl_input(Vtl::Vtl2).unwrap()
            },
            EnablePartitionVtl {
                flags: EnablePartitionVtlFlags::from(0x80),
                ..partition_vtl_input(Vtl::Vtl2).unwrap()
            },
        ] {
            let mut args = mshv_root_hvcall {
                code: HypercallCode::HvCallEnablePartitionVtl.0,
                in_sz: size_of::<EnablePartitionVtl>() as u16,
                in_ptr: std::ptr::addr_of!(input) as u64,
                ..Default::default()
            };
            assert_eq!(vmfd.hvcall(&mut args).unwrap_err().errno(), libc::EINVAL);
            assert_eq!(args.status, 0);
        }

        for input in [
            EnableVpVtlX64 {
                target_vtl: 0,
                ..vp_vtl_input(0, Vtl::Vtl2, reset_context()).unwrap()
            },
            EnableVpVtlX64 {
                reserved: [1, 0, 0],
                ..vp_vtl_input(0, Vtl::Vtl2, reset_context()).unwrap()
            },
            vp_vtl_input(u32::MAX, Vtl::Vtl2, reset_context()).unwrap(),
            vp_vtl_input(1, Vtl::Vtl2, reset_context()).unwrap(),
        ] {
            let mut args = mshv_root_hvcall {
                code: HypercallCode::HvCallEnableVpVtl.0,
                in_sz: size_of::<EnableVpVtlX64>() as u16,
                in_ptr: std::ptr::addr_of!(input) as u64,
                ..Default::default()
            };
            assert_eq!(vmfd.hvcall(&mut args).unwrap_err().errno(), libc::EINVAL);
            assert_eq!(args.status, 0);
        }

        let input = partition_vtl_input(Vtl::Vtl2).unwrap();
        for (in_sz, reps) in [(8, 0), (16, 1)] {
            let mut args = mshv_root_hvcall {
                code: HypercallCode::HvCallEnablePartitionVtl.0,
                in_sz,
                reps,
                in_ptr: std::ptr::addr_of!(input) as u64,
                ..Default::default()
            };
            assert_eq!(vmfd.hvcall(&mut args).unwrap_err().errno(), libc::EINVAL);
            assert_eq!(args.status, 0);
        }
    }

    #[test]
    #[ignore = "requires a VSM-enabled MSHV host and access to /dev/mshv"]
    fn vsm_native_vtl2_probe() {
        let result = probe_vtl2().expect("native child-partition VTL2 probe failed");
        println!("{result:?}");
    }
}
