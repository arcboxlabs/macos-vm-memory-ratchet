// vz-ratchet harness: a dumb bridge around one Virtualization.framework
// Linux VM. All measurement lives in the Rust driver (crates/vz-ratchet);
// this process only boots the VM, forwards the serial console, and sets
// the balloon target on request.
//
//   argv:   <kernel> <initramfs> <memory-mib>
//   stdin:  "guest <line>"  -> guest serial input
//           "balloon <MiB>" -> balloon targetVirtualMachineMemorySize
//           "quit"          -> exit (tears the VM down)
//   stdout: guest serial output verbatim, plus "HARNESS <event>" lines.

import Foundation
import Virtualization

guard CommandLine.arguments.count == 4,
  let memMiB = UInt64(CommandLine.arguments[3])
else {
  FileHandle.standardError.write(Data("usage: vz-harness <kernel> <initramfs> <memory-mib>\n".utf8))
  exit(2)
}

func emit(_ s: String) {
  FileHandle.standardOutput.write(Data(("HARNESS " + s + "\n").utf8))
}

let toGuest = Pipe()
let fromGuest = Pipe()

let boot = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: CommandLine.arguments[1]))
boot.initialRamdiskURL = URL(fileURLWithPath: CommandLine.arguments[2])
boot.commandLine = "console=hvc0 rdinit=/init"

let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
serial.attachment = VZFileHandleSerialPortAttachment(
  fileHandleForReading: toGuest.fileHandleForReading,
  fileHandleForWriting: fromGuest.fileHandleForWriting)

let config = VZVirtualMachineConfiguration()
config.bootLoader = boot
config.cpuCount = 2
config.memorySize = memMiB << 20
config.serialPorts = [serial]
config.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]

do { try config.validate() } catch {
  emit("config-invalid \(error)")
  exit(1)
}

let vm = VZVirtualMachine(configuration: config, queue: .main)

fromGuest.fileHandleForReading.readabilityHandler = { handle in
  let data = handle.availableData
  if !data.isEmpty { FileHandle.standardOutput.write(data) }
}

// Runs on the main queue — the VM's queue — so device access is legal.
func handle(_ line: String) {
  if line.hasPrefix("guest ") {
    toGuest.fileHandleForWriting.write(Data((line.dropFirst(6) + "\n").utf8))
  } else if line.hasPrefix("balloon ") {
    guard let mib = UInt64(line.dropFirst(8)),
      let device = vm.memoryBalloonDevices.first as? VZVirtioTraditionalMemoryBalloonDevice
    else {
      emit("balloon-error")
      return
    }
    device.targetVirtualMachineMemorySize = mib << 20
    emit("balloon-target \(mib)")
  } else if line == "quit" {
    exit(0)
  }
}

Thread {
  while let line = readLine(strippingNewline: true) {
    DispatchQueue.main.async { handle(line) }
  }
  DispatchQueue.main.async { exit(0) }
}.start()

DispatchQueue.main.async {
  vm.start { result in
    if case .failure(let error) = result {
      emit("start-failed \(error)")
      exit(1)
    }
    emit("started")
  }
}

RunLoop.main.run()
