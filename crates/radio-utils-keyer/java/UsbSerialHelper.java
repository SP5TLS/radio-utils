package com.radioutils.serial;

import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.hardware.usb.UsbConstants;
import android.hardware.usb.UsbDevice;
import android.hardware.usb.UsbDeviceConnection;
import android.hardware.usb.UsbEndpoint;
import android.hardware.usb.UsbInterface;
import android.hardware.usb.UsbManager;
import android.os.Build;
import android.util.Log;
import java.util.Locale;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * USB-serial helper for the keyer's modem-line paddle input. Supports four
 * chip families directly via {@link UsbDeviceConnection}:
 *
 * <ul>
 *   <li><b>FTDI</b> (VID 0x0403) — modem status via vendor control transfer
 *       (bRequest 0x05, GET_MODEM_STATUS).</li>
 *   <li><b>CH340/CH341</b> (VID 0x1a86) — vendor READ_REG (bRequest 0x95).</li>
 *   <li><b>CP210x</b> (VID 0x10c4) — vendor GET_MDMSTS (bRequest 0x08).</li>
 *   <li><b>CDC-ACM</b> (any VID; interface class 0x02 subclass 0x02) —
 *       asynchronous {@code SERIAL_STATE} notifications on the comm
 *       interrupt endpoint (bNotification 0x20). Covers Arduino Leonardo,
 *       RP2040 native USB, STM32 USB-CDC, and most "USB Serial" microcontrollers.</li>
 * </ul>
 *
 * <p>We do not link a third-party usb-serial library. For the vendor chips
 * the keyer only needs to <em>read</em> the modem-status pins (CTS/DSR/DCD/RI),
 * which is a single control transfer. For CDC-ACM the chip pushes state
 * changes via the interrupt endpoint, so we keep a cached last-known state
 * and update it whenever a notification arrives.
 *
 * <p>The native side (Rust {@code serial_android.rs}) holds an instance of
 * this class and calls {@link #readModemStatus()} in a tight polling loop
 * on a permanently-attached JNI thread.
 */
public class UsbSerialHelper {
    private static final String TAG = "UsbSerialHelper";
    private static final String ACTION_USB_PERMISSION = "com.radioutils.serial.USB_PERMISSION";

    private static final int VID_FTDI   = 0x0403;
    private static final int VID_CH340  = 0x1a86;
    private static final int VID_CP210X = 0x10c4;

    /** Chip family. */
    private static final int CHIP_UNSUPPORTED = 0;
    private static final int CHIP_FTDI        = 1;
    private static final int CHIP_CH340       = 2;
    private static final int CHIP_CP210X      = 3;
    private static final int CHIP_CDC_ACM     = 4;

    /** Modem-status bitmask returned by {@link #readModemStatus()}. */
    public static final int CTS = 1;
    public static final int DSR = 2;
    public static final int DCD = 4;
    public static final int RI  = 8;

    /** USB CDC class codes (USB-IF Class Definitions for Communication Devices). */
    private static final int USB_CLASS_CDC_COMM  = 0x02;
    private static final int USB_CLASS_CDC_DATA  = 0x0a;
    private static final int CDC_SUBCLASS_ACM    = 0x02;

    /** CDC-ACM control requests (bmRequestType 0x21). */
    private static final int CDC_SET_LINE_CODING        = 0x20;
    private static final int CDC_SET_CONTROL_LINE_STATE = 0x22;
    /** Notification opcode for SerialState (bmRequestType 0xA1, bNotification 0x20). */
    private static final int CDC_NOTIFY_SERIAL_STATE    = 0x20;

    /** Match Context.RECEIVER_NOT_EXPORTED without compiling against API 33+. */
    private static final int RECEIVER_NOT_EXPORTED = 0x4;

    private UsbDeviceConnection connection;
    private UsbInterface iface;        // primary / vendor / CDC comm
    private UsbInterface dataIface;    // CDC-ACM data interface, null for vendor chips
    private UsbEndpoint interruptEp;   // CDC-ACM interrupt-in, null for vendor chips
    private int chip;
    private int interfaceNum;

    /** Last UART_STATE byte received from a CDC-ACM SerialState notification. */
    private volatile int cdcLastUartState = 0;
    /** Last decoded modem-status bitmask for FTDI/CH340/CP210x — change-detect logging. */
    private int vendorLastStatus = -1;
    /** Reusable scratch buffer for synchronous interrupt-IN reads. */
    private byte[] cdcRxBuf;
    /** Set by close() to short-circuit the poll loop. */
    private volatile boolean cdcClosing = false;
    /** Timestamp of the last log we emitted from readCdcAcm, for heartbeat throttling. */
    private long cdcLastHeartbeatMs = 0;

    /**
     * Names of supported USB-serial devices currently attached. Format:
     * {@code "<product> [VID:PID]"} — matches what {@link #openDevice} expects.
     */
    public static String[] listDevices(Context ctx) {
        UsbManager um = (UsbManager) ctx.getSystemService(Context.USB_SERVICE);
        if (um == null) return new String[0];
        java.util.ArrayList<String> out = new java.util.ArrayList<String>();
        for (UsbDevice dev : um.getDeviceList().values()) {
            if (detectChip(dev) != CHIP_UNSUPPORTED) {
                out.add(displayName(dev));
            }
        }
        return out.toArray(new String[0]);
    }

    /**
     * Find the device with this display name, request USB permission if
     * needed (blocking up to 10 s for the user to tap the system dialog),
     * open the connection, claim the relevant interface(s), and run chip-
     * specific init so the modem-status read returns sensible values.
     *
     * <p>Returns {@code null} on any failure.
     */
    public static UsbSerialHelper openDevice(Context ctx, String name) {
        UsbManager um = (UsbManager) ctx.getSystemService(Context.USB_SERVICE);
        if (um == null) return null;

        UsbDevice target = null;
        for (UsbDevice dev : um.getDeviceList().values()) {
            if (displayName(dev).equals(name)) { target = dev; break; }
        }
        if (target == null) return null;

        int kind = detectChip(target);
        if (kind == CHIP_UNSUPPORTED) return null;

        if (!um.hasPermission(target) && !requestPermissionBlocking(ctx, um, target)) {
            Log.w(TAG, "USB permission denied for " + name);
            return null;
        }

        UsbDeviceConnection conn = um.openDevice(target);
        if (conn == null) {
            Log.w(TAG, "openDevice returned null for " + name);
            return null;
        }

        UsbSerialHelper h = new UsbSerialHelper();
        h.connection = conn;
        h.chip = kind;

        boolean ok;
        if (kind == CHIP_CDC_ACM) {
            ok = h.openCdcAcm(target);
        } else {
            ok = h.openVendor(target);
        }
        if (!ok) {
            h.close();
            return null;
        }

        if (!h.init()) {
            Log.w(TAG, "Chip init failed for " + name);
            h.close();
            return null;
        }
        return h;
    }

    /** Claim the single vendor-protocol interface (FTDI/CH340/CP210x). */
    private boolean openVendor(UsbDevice dev) {
        if (dev.getInterfaceCount() < 1) return false;
        iface = dev.getInterface(0);
        interfaceNum = iface.getId();
        if (!connection.claimInterface(iface, true)) {
            Log.w(TAG, "claimInterface failed (vendor)");
            return false;
        }
        return true;
    }

    /**
     * CDC-ACM open: locate the comm interface (class 2 / subclass 2), its
     * interrupt-in endpoint, and the data interface (class 0x0a). Claim both
     * — the OLD heap-buffer build that did this delivered a real 10-byte
     * SerialState on the first completion, and the variants without the data
     * claim get only zero-byte completions. Best guess: this RP2040 embassy
     * stack only services the comm endpoint once the full CDC function's
     * interfaces are claimed.
     *
     * <p>No {@code setInterface()} call here. We tried it; it makes URBs
     * complete with 0 bytes (probably because SET_INTERFACE resets the
     * endpoint state and embassy-usb stops emitting data on the interrupt
     * endpoint until it's renegotiated).
     */
    private boolean openCdcAcm(UsbDevice dev) {
        UsbInterface comm = null;
        UsbInterface data = null;
        UsbEndpoint intrEp = null;
        for (int i = 0; i < dev.getInterfaceCount(); i++) {
            UsbInterface candidate = dev.getInterface(i);
            int cls = candidate.getInterfaceClass();
            int sub = candidate.getInterfaceSubclass();
            if (comm == null && cls == USB_CLASS_CDC_COMM && sub == CDC_SUBCLASS_ACM) {
                comm = candidate;
                for (int j = 0; j < candidate.getEndpointCount(); j++) {
                    UsbEndpoint ep = candidate.getEndpoint(j);
                    if (ep.getType() == UsbConstants.USB_ENDPOINT_XFER_INT
                            && ep.getDirection() == UsbConstants.USB_DIR_IN) {
                        intrEp = ep;
                        break;
                    }
                }
            } else if (data == null && cls == USB_CLASS_CDC_DATA) {
                data = candidate;
            }
        }
        if (comm == null || intrEp == null) {
            Log.w(TAG, "CDC-ACM: comm interface or interrupt-in endpoint missing");
            return false;
        }
        if (!connection.claimInterface(comm, true)) {
            Log.w(TAG, "CDC-ACM: claim comm failed");
            return false;
        }
        if (data != null) {
            boolean dataOk = connection.claimInterface(data, true);
            Log.i(TAG, "CDC-ACM claim data iface (" + data.getId() + ") = " + dataOk);
        }

        iface = comm;
        dataIface = data;
        interruptEp = intrEp;
        interfaceNum = comm.getId();
        cdcRxBuf = new byte[Math.max(16, intrEp.getMaxPacketSize())];

        Log.i(TAG, "CDC-ACM opened: commIface=" + comm.getId()
                + " intrEp.addr=0x" + Integer.toHexString(intrEp.getAddress())
                + " intrEp.maxPacket=" + intrEp.getMaxPacketSize()
                + " intrEp.bInterval=" + intrEp.getInterval()
                + " bufSize=" + cdcRxBuf.length);
        return true;
    }

    /**
     * Read modem-status pins. Returns a bitmask of {@link #CTS}, {@link #DSR},
     * {@link #DCD}, {@link #RI}, or {@code -1} on transfer error.
     *
     * <p>For vendor chips this issues one control transfer (~1 ms on USB-OTG
     * full-speed). For CDC-ACM this blocks up to 100 ms on the interrupt
     * endpoint waiting for a SerialState notification; if none arrives in
     * that window we return the cached last-known state (no change since
     * the previous notification).
     */
    public int readModemStatus() {
        if (connection == null) return -1;
        switch (chip) {
            case CHIP_FTDI: {
                byte[] buf = new byte[2];
                int n = connection.controlTransfer(0xC0, 0x05, 0, 0, buf, 2, 100);
                if (n < 1) return -1;
                int s = buf[0] & 0xff;
                int r = 0;
                if ((s & 0x10) != 0) r |= CTS;
                if ((s & 0x20) != 0) r |= DSR;
                if ((s & 0x80) != 0) r |= DCD;
                if ((s & 0x40) != 0) r |= RI;
                logVendorChange("FTDI", s, r);
                return r;
            }
            case CHIP_CH340: {
                byte[] buf = new byte[2];
                int n = connection.controlTransfer(0xC0, 0x95, 0x0706, 0, buf, 2, 100);
                if (n < 1) return -1;
                int s = (~buf[0]) & 0x0f;
                int r = 0;
                if ((s & 0x01) != 0) r |= CTS;
                if ((s & 0x02) != 0) r |= DSR;
                if ((s & 0x08) != 0) r |= DCD;
                if ((s & 0x04) != 0) r |= RI;
                logVendorChange("CH340", buf[0] & 0xff, r);
                return r;
            }
            case CHIP_CP210X: {
                byte[] buf = new byte[1];
                int n = connection.controlTransfer(0xC1, 0x08, 0, interfaceNum, buf, 1, 100);
                if (n < 1) return -1;
                int s = buf[0] & 0xff;
                int r = 0;
                if ((s & 0x10) != 0) r |= CTS;
                if ((s & 0x20) != 0) r |= DSR;
                if ((s & 0x80) != 0) r |= DCD;
                if ((s & 0x40) != 0) r |= RI;
                logVendorChange("CP210x", s, r);
                return r;
            }
            case CHIP_CDC_ACM:
                return readCdcAcm();
            default:
                return -1;
        }
    }

    /**
     * Log a one-line summary of the modem-status bitmask, but only when it
     * differs from the previous read. Lets the user see "press paddle → DCD
     * went high" in logcat to verify their wiring (esp. useful when "RTS"
     * doesn't work — RTS is host-driven, paddles physically can't pull it).
     */
    private void logVendorChange(String chipTag, int rawByte, int decoded) {
        if (decoded == vendorLastStatus) return;
        vendorLastStatus = decoded;
        Log.i(TAG, String.format(Locale.US,
                "%s modem status: raw=0x%02x  CTS=%d DSR=%d DCD=%d RI=%d",
                chipTag, rawByte,
                (decoded & CTS) != 0 ? 1 : 0,
                (decoded & DSR) != 0 ? 1 : 0,
                (decoded & DCD) != 0 ? 1 : 0,
                (decoded & RI)  != 0 ? 1 : 0));
    }

    /**
     * One iteration of the CDC-ACM SerialState polling loop. Reads the
     * interrupt-IN endpoint with a 100 ms timeout via the synchronous
     * {@link UsbDeviceConnection#bulkTransfer} path. Returns the cached
     * UART state mapped to our bitmask.
     *
     * <p>We tried the async {@link UsbRequest} path twice — first reusing
     * one request, then recreating per completion, with both heap and direct
     * ByteBuffers — and on this device (Pixel 10 Pro XL on Tensor G5 with
     * Android 14's USB stack) every URB came back with 0 bytes regardless.
     * Synchronous {@code bulkTransfer} on the interrupt EP, while technically
     * "documented for bulk endpoints only", goes straight to the same
     * USBDEVFS_BULK ioctl that does the URB inside the kernel and produces
     * the actual data; that's the path every long-running Android USB-serial
     * library actually ships with.
     *
     * <p>CTS is not exposed by CDC-ACM SerialState — paddles intended for
     * CDC-ACM should be wired to DSR, DCD, or RI.
     */
    private int readCdcAcm() {
        if (cdcClosing || connection == null || interruptEp == null) return mapCdcState();

        int n;
        try {
            n = connection.bulkTransfer(interruptEp, cdcRxBuf, cdcRxBuf.length, 100);
        } catch (Exception e) {
            return mapCdcState();
        }

        long now = System.currentTimeMillis();
        if (n <= 0) {
            // Negative = error/timeout, zero = empty packet. Throttle the
            // heartbeat to every 2 s so we can confirm the loop is alive
            // without spamming the log.
            if (now - cdcLastHeartbeatMs >= 2000) {
                cdcLastHeartbeatMs = now;
                Log.i(TAG, "CDC-ACM polling (no data; last bulkTransfer rc=" + n
                        + "; cached state=0x" + Integer.toHexString(cdcLastUartState) + ")");
            }
            return mapCdcState();
        }
        cdcLastHeartbeatMs = now;

        StringBuilder sb = new StringBuilder(n * 3);
        for (int i = 0; i < n; i++) {
            sb.append(String.format(Locale.US, "%02x ", cdcRxBuf[i] & 0xff));
        }
        Log.i(TAG, "CDC-ACM interrupt rx (" + n + " bytes): " + sb.toString());

        if (n >= 10) {
            int bmRequestType = cdcRxBuf[0] & 0xff;
            int bNotification = cdcRxBuf[1] & 0xff;
            if (bmRequestType == 0xa1 && bNotification == CDC_NOTIFY_SERIAL_STATE) {
                int prev = cdcLastUartState;
                cdcLastUartState = cdcRxBuf[8] & 0xff;
                if (cdcLastUartState != prev) {
                    Log.i(TAG, String.format(Locale.US,
                            "CDC-ACM SerialState: 0x%02x (DCD=%d DSR=%d BRK=%d RI=%d)",
                            cdcLastUartState,
                            (cdcLastUartState & 0x01),
                            (cdcLastUartState >> 1) & 0x01,
                            (cdcLastUartState >> 2) & 0x01,
                            (cdcLastUartState >> 3) & 0x01));
                }
            } else {
                Log.i(TAG, String.format(Locale.US,
                        "CDC-ACM interrupt: unrecognised header bmReq=0x%02x bNotify=0x%02x",
                        bmRequestType, bNotification));
            }
        }

        return mapCdcState();
    }

    /**
     * Translate the cached CDC UART_STATE byte to our CTS/DSR/DCD/RI bitmask.
     * UART_STATE byte 0 layout from CDC PSTN 1.2 §6.5.4:
     *   bit 0 = bRxCarrier (DCD), bit 1 = bTxCarrier (DSR),
     *   bit 2 = bBreak,           bit 3 = bRingSignal (RI).
     */
    private int mapCdcState() {
        int s = cdcLastUartState;
        int r = 0;
        if ((s & 0x02) != 0) r |= DSR;
        if ((s & 0x01) != 0) r |= DCD;
        if ((s & 0x08) != 0) r |= RI;
        return r;
    }

    /** Release the interface and close the connection. Idempotent. */
    public void close() {
        cdcClosing = true;
        cdcRxBuf = null;
        try {
            if (connection != null && iface != null) connection.releaseInterface(iface);
        } catch (Exception ignored) {}
        try {
            if (connection != null && dataIface != null) connection.releaseInterface(dataIface);
        } catch (Exception ignored) {}
        try {
            if (connection != null) connection.close();
        } catch (Exception ignored) {}
        connection = null;
        iface = null;
        dataIface = null;
        interruptEp = null;
    }

    /**
     * Initialise the chip far enough that modem-status reads work. We pin
     * baud=9600 8N1 and raise DTR+RTS so paddle adapters that source the
     * switch closure from DTR or RTS see a live driver line. For CDC-ACM
     * we additionally rely on the device sending an initial SerialState
     * once SET_CONTROL_LINE_STATE is acknowledged.
     */
    private boolean init() {
        switch (chip) {
            case CHIP_FTDI: {
                if (ctrlOut(0x40, 0x00, 0x0000, 0) < 0) return false;
                if (ctrlOut(0x40, 0x03, 0x4138, 0) < 0) return false;
                if (ctrlOut(0x40, 0x04, 0x0008, 0) < 0) return false;
                if (ctrlOut(0x40, 0x01, 0x0303, 0) < 0) return false;
                return true;
            }
            case CHIP_CH340: {
                if (ctrlOut(0x40, 0xA1, 0xC29C, 0xB2B9) < 0) return false;
                if (ctrlOut(0x40, 0xA4, 0x009F, 0) < 0) return false;
                return true;
            }
            case CHIP_CP210X: {
                if (ctrlOut(0x41, 0x00, 0x0001, interfaceNum) < 0) return false;
                byte[] baud = { (byte) 0x80, 0x25, 0x00, 0x00 };
                if (connection.controlTransfer(0x41, 0x1E, 0, interfaceNum, baud, 4, 1000) < 0) {
                    return false;
                }
                if (ctrlOut(0x41, 0x03, 0x0800, interfaceNum) < 0) return false;
                if (ctrlOut(0x41, 0x07, 0x0303, interfaceNum) < 0) return false;
                return true;
            }
            case CHIP_CDC_ACM: {
                // SET_LINE_CODING — 7 bytes: dwDTERate (4) | bCharFormat (1) |
                // bParityType (1) | bDataBits (1). 9600 8N1.
                byte[] line = {
                    (byte) 0x80, 0x25, 0x00, 0x00, // 9600 baud (0x2580) LE
                    0x00,                          // 1 stop bit
                    0x00,                          // no parity
                    0x08                           // 8 data bits
                };
                int rc = connection.controlTransfer(0x21, CDC_SET_LINE_CODING, 0,
                        interfaceNum, line, line.length, 1000);
                Log.i(TAG, "CDC-ACM SET_LINE_CODING rc=" + rc);
                // SET_CONTROL_LINE_STATE — value bit 0 = DTR, bit 1 = RTS.
                rc = connection.controlTransfer(0x21, CDC_SET_CONTROL_LINE_STATE,
                        0x0003, interfaceNum, null, 0, 1000);
                Log.i(TAG, "CDC-ACM SET_CONTROL_LINE_STATE rc=" + rc);
                return true;
            }
            default:
                return false;
        }
    }

    private int ctrlOut(int reqType, int req, int value, int index) {
        return connection.controlTransfer(reqType, req, value, index, null, 0, 1000);
    }

    /**
     * Identify the chip family. Vendor IDs are checked first (cheap, exact);
     * if no match, scan interfaces for a CDC-ACM signature (class 2 /
     * subclass 2). This catches composite IAD devices that report device
     * class 0xEF (Misc) at the top level.
     */
    private static int detectChip(UsbDevice dev) {
        int vid = dev.getVendorId();
        if (vid == VID_FTDI)   return CHIP_FTDI;
        if (vid == VID_CH340)  return CHIP_CH340;
        if (vid == VID_CP210X) return CHIP_CP210X;
        for (int i = 0; i < dev.getInterfaceCount(); i++) {
            UsbInterface candidate = dev.getInterface(i);
            if (candidate.getInterfaceClass() == USB_CLASS_CDC_COMM
                    && candidate.getInterfaceSubclass() == CDC_SUBCLASS_ACM) {
                return CHIP_CDC_ACM;
            }
        }
        return CHIP_UNSUPPORTED;
    }

    private static String displayName(UsbDevice dev) {
        String n = null;
        try {
            n = dev.getProductName();
        } catch (Exception ignored) {}
        if (n == null || n.isEmpty()) n = "USB Serial";
        return String.format(Locale.US, "%s [%04x:%04x]",
                n, dev.getVendorId(), dev.getProductId());
    }

    /**
     * Pop the system USB-permission dialog and block until the user responds
     * (or 3 s elapse). Safe to call from a background thread.
     *
     * <p>3 s is a deliberate trade-off: this call is reached on the radio
     * actor's command-loop thread (see {@code build_keyer_input} in
     * the radio crate), so blocking it stalls every other
     * radio command for the duration. A user who taps "Allow" reacts well
     * within 3 s; one who hesitates can re-select the port from the keyer
     * settings panel after granting permission — Android already remembers
     * the choice, so the second {@code openDevice} call hits the
     * {@code hasPermission} fast path.
     */
    private static boolean requestPermissionBlocking(Context ctx, UsbManager um, UsbDevice dev) {
        final CountDownLatch latch = new CountDownLatch(1);
        final boolean[] granted = { false };

        BroadcastReceiver receiver = new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent intent) {
                if (!ACTION_USB_PERMISSION.equals(intent.getAction())) return;
                granted[0] = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false);
                latch.countDown();
            }
        };

        int flags = 0;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            flags = 0x02000000; // PendingIntent.FLAG_MUTABLE
        }
        Intent intent = new Intent(ACTION_USB_PERMISSION).setPackage(ctx.getPackageName());
        PendingIntent pi = PendingIntent.getBroadcast(ctx, 0, intent, flags);

        IntentFilter filter = new IntentFilter(ACTION_USB_PERMISSION);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            ctx.registerReceiver(receiver, filter, RECEIVER_NOT_EXPORTED);
        } else {
            ctx.registerReceiver(receiver, filter);
        }

        try {
            um.requestPermission(dev, pi);
            latch.await(3, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            return false;
        } finally {
            try { ctx.unregisterReceiver(receiver); } catch (Exception ignored) {}
        }
        return granted[0];
    }
}
