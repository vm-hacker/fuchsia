// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_HYPERVISOR_VMX_CPU_STATE_PRIV_H_
#define ZIRCON_KERNEL_ARCH_X86_HYPERVISOR_VMX_CPU_STATE_PRIV_H_

#include <arch/hypervisor.h>

// clang-format off

#define X86_MSR_IA32_FEATURE_CONTROL        0x003a // Feature control
#define X86_MSR_IA32_VMX_BASIC              0x0480 // Basic info
#define X86_MSR_IA32_VMX_CR0_FIXED0         0x0486 // CR0 bits that must be 0 to enter VMX
#define X86_MSR_IA32_VMX_CR0_FIXED1         0x0487 // CR0 bits that must be 1 to enter VMX
#define X86_MSR_IA32_VMX_CR4_FIXED0         0x0488 // CR4 bits that must be 0 to enter VMX
#define X86_MSR_IA32_VMX_CR4_FIXED1         0x0489 // CR4 bits that must be 1 to enter VMX
#define X86_MSR_IA32_VMX_EPT_VPID_CAP       0x048c // VPID and EPT Capabilities
#define X86_MSR_IA32_VMX_MISC               0x0485 // Miscellaneous info

/* X86_MSR_IA32_VMX_BASIC flags */
#define VMX_MEMORY_TYPE_WRITE_BACK          0x06 // Write back

/* X86_MSR_IA32_FEATURE_CONTROL flags */
#define X86_MSR_IA32_FEATURE_CONTROL_LOCK   (1u << 0) // Locked
#define X86_MSR_IA32_FEATURE_CONTROL_VMXON  (1u << 2) // Enable VMXON

// clang-format on

/* Stores VMX info from the IA32_VMX_BASIC MSR. */
struct VmxInfo {
  uint32_t revision_id;
  uint16_t region_size;
  bool write_back;
  bool io_exit_info;
  bool vmx_controls;

  VmxInfo();
};

/* Stores EPT info from the IA32_VMX_EPT_VPID_CAP MSR. */
struct EptInfo {
  bool page_walk_4;
  bool write_back;
  bool large_pages;
  bool invept;
  bool invvpid;

  EptInfo();
};

/* VMX region to be used with both VMXON and VMCS. */
struct VmxRegion {
  uint32_t revision_id;
};

// INVEPT invalidation types.
//
// From Volume 3, Section 30.3: There are two INVEPT types currently defined:
// * Single-context invalidation. If the INVEPT type is 1, the logical
//   processor invalidates all mappings associated with bits 51:12 of the EPT
//   pointer (EPTP) specified in the INVEPT descriptor. It may invalidate other
//   mappings as well.
// * Global invalidation. If the INVEPT type is 2, the logical processor
//   invalidates mappings associated with all EPTPs.
enum class InvEpt : uint64_t {
  SINGLE_CONTEXT = 1,
  GLOBAL = 2,
};

void invept(InvEpt invalidation, uint64_t eptp);
uint64_t ept_pointer_from_pml4(paddr_t pml4_address);
zx::status<> alloc_vmx_state();
void free_vmx_state();
bool cr_is_invalid(uint64_t cr_value, uint32_t fixed0_msr, uint32_t fixed1_msr);

#endif  // ZIRCON_KERNEL_ARCH_X86_HYPERVISOR_VMX_CPU_STATE_PRIV_H_
