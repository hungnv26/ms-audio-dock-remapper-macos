/* Software "replug" of a USB device through IOUSBLib: asks the host controller
 * to re-enumerate the device with the given vendor/product id, which is what
 * unplugging and replugging it would do. Hubs are owned by the kernel and
 * refuse to be opened, so this only works on end devices such as the Dock's
 * audio/HID function. Compiled by build.rs on macOS. */
#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/IOCFPlugIn.h>
#include <IOKit/usb/IOUSBLib.h>

/* Returns 0 on success, 1 when no such device is on the bus, otherwise the
 * IOKit error code of the failing step (open or re-enumerate). */
int msadr_usb_reenumerate(unsigned short vid, unsigned short pid) {
    CFMutableDictionaryRef matching = IOServiceMatching(kIOUSBDeviceClassName);
    if (!matching) return kIOReturnNoMemory;
    int v = vid, p = pid;
    CFNumberRef cv = CFNumberCreate(NULL, kCFNumberIntType, &v);
    CFNumberRef cp = CFNumberCreate(NULL, kCFNumberIntType, &p);
    CFDictionarySetValue(matching, CFSTR(kUSBVendorID), cv);
    CFDictionarySetValue(matching, CFSTR(kUSBProductID), cp);
    CFRelease(cv);
    CFRelease(cp);

    io_iterator_t iterator = 0;
    if (IOServiceGetMatchingServices(kIOMainPortDefault, matching, &iterator) != KERN_SUCCESS) {
        return kIOReturnNotFound;
    }
    io_service_t service = IOIteratorNext(iterator);
    IOObjectRelease(iterator);
    if (!service) return 1;

    IOCFPlugInInterface **plugin = NULL;
    SInt32 score = 0;
    kern_return_t kr = IOCreatePlugInInterfaceForService(
        service, kIOUSBDeviceUserClientTypeID, kIOCFPlugInInterfaceID, &plugin, &score);
    IOObjectRelease(service);
    if (kr != KERN_SUCCESS || !plugin) return kr ? kr : kIOReturnError;

    IOUSBDeviceInterface **device = NULL;
    (*plugin)->QueryInterface(plugin, CFUUIDGetUUIDBytes(kIOUSBDeviceInterfaceID), (LPVOID *)&device);
    (*plugin)->Release(plugin);
    if (!device) return kIOReturnUnsupported;

    kr = (*device)->USBDeviceOpen(device);
    if (kr == kIOReturnSuccess) {
        kr = (*device)->USBDeviceReEnumerate(device, 0);
        (*device)->USBDeviceClose(device);
    }
    (*device)->Release(device);
    return kr;
}
