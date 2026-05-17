package com.radioutils.midi;

import android.content.Context;
import android.media.midi.*;
import android.os.Handler;
import java.io.IOException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Thin Java wrapper for Android MIDI device discovery and opening.
 * MIDI data is received natively via AMidi (AMidiInputPort_receive),
 * so this class no longer extends MidiReceiver or uses any Java callback.
 */
public class MidiHelper {
    private MidiDevice device;

    /**
     * List names of MIDI devices that have at least one output port (i.e. devices
     * that send data to the host — what Android calls TYPE_OUTPUT). Devices with
     * only input ports (receive-only) are excluded because the native AMidi path
     * can only open output ports.
     */
    public static String[] listDevices(Context ctx) {
        MidiManager mm = (MidiManager) ctx.getSystemService(Context.MIDI_SERVICE);
        if (mm == null) return new String[0];
        MidiDeviceInfo[] infos = mm.getDevices();
        java.util.List<String> names = new java.util.ArrayList<>();
        for (int i = 0; i < infos.length; i++) {
            boolean hasOutputPort = false;
            for (MidiDeviceInfo.PortInfo portInfo : infos[i].getPorts()) {
                if (portInfo.getType() == MidiDeviceInfo.PortInfo.TYPE_OUTPUT) {
                    hasOutputPort = true;
                    break;
                }
            }
            if (!hasOutputPort) continue;
            String name = infos[i].getProperties()
                .getString(MidiDeviceInfo.PROPERTY_NAME);
            names.add((name != null) ? name : "MIDI Device " + i);
        }
        return names.toArray(new String[0]);
    }

    /**
     * Open a MIDI device by name. Blocks up to 1 second.
     * Returns null if not found or open fails.
     * The native layer calls getMidiDevice() and uses AMidiDevice_fromJava()
     * to obtain a native handle for polling via AMidiInputPort_receive().
     */
    public static MidiHelper openDevice(Context ctx, String name) throws IOException {
        MidiManager mm = (MidiManager) ctx.getSystemService(Context.MIDI_SERVICE);
        if (mm == null) return null;

        MidiDeviceInfo target = null;
        for (MidiDeviceInfo info : mm.getDevices()) {
            String n = info.getProperties().getString(MidiDeviceInfo.PROPERTY_NAME);
            if (name.equals(n)) { target = info; break; }
        }
        if (target == null) return null;

        // Verify device has at least one output port before opening.
        boolean hasOutputPort = false;
        for (MidiDeviceInfo.PortInfo portInfo : target.getPorts()) {
            if (portInfo.getType() == MidiDeviceInfo.PortInfo.TYPE_OUTPUT) {
                hasOutputPort = true;
                break;
            }
        }
        if (!hasOutputPort) return null;

        final MidiDevice[] result = { null };
        final CountDownLatch latch = new CountDownLatch(1);
        // Use a dedicated HandlerThread so the callback can fire regardless of which
        // thread is calling this method (avoids deadlock when called from the main thread).
        android.os.HandlerThread ht = new android.os.HandlerThread("midi-open");
        ht.start();
        try {
            mm.openDevice(target, dev -> { result[0] = dev; latch.countDown(); },
                          new Handler(ht.getLooper()));
            try { latch.await(1, TimeUnit.SECONDS); }
            catch (InterruptedException e) { return null; }
        } finally {
            ht.quit();
        }
        if (result[0] == null) return null;

        MidiHelper helper = new MidiHelper();
        helper.device = result[0];
        return helper;
    }

    /**
     * Return the underlying MidiDevice so native code can call AMidiDevice_fromJava().
     * The device must remain open (this object must stay alive) for as long as the
     * native AMidiDevice handle is in use.
     */
    public MidiDevice getMidiDevice() { return device; }

    /** Release resources. */
    public void close() throws IOException {
        if (device != null) { device.close(); device = null; }
    }
}
