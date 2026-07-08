<img width="144" height="144" alt="rvci" src="https://github.com/user-attachments/assets/d20da90c-899d-4941-a34f-7a460fd544d3" /> 

# RVCI

RVCI (Rust Volume Control Interface) is a hardware-software project which can control audio devices as well as volumes in W10/11 using basic hardware

**Showcase**

[Youtube Showcase Video](https://youtu.be/tl9Kyg_MPLU)


This project is currently work in progress. Current repo files might or might not work properly. This version includes a fully working GUI for configuration, OSD for seeing volume changes, volume control functionality for output and microphones, volume curves, an audio output switcher, virtual/output device volume mapping (e.g. SteelSeries Sonar channels), and configurable hardware buttons (mute a knob, media keys, or macropad-style key combos).
RVCI was heavily inspired by Deej, however, Deej is no longer maintained and recent Windows updates heavily broke its functionality. The Idea stems from my forked version of deej "DeejXChanger", but I decided to write a new and improved lightweight version of it in Rust, and am adding the functionality I wish Deej had. 
RVCI runs on only ~2MB Ram and very little CPU.

Also, I hated writing in Go

## GUI:

The GUI lets you intuitively create, modify and delete knob mappings. Each knob can be mapped to your **System** (master) volume, a single **Process**, all **Others** (every unmapped app), a **Microphone** (capture device), or an **Output Device** (a specific playback endpoint, including virtual outputs such as SteelSeries Sonar's Game/Chat/Media/Aux channels). Furthermore, you can easily change COM ports, Baudrate as well as between what Audio Outputs the device should switch between using a physical switch connected to the Arduino. 
Furthermore, you can choose between a linear volume curve, or a logarithmic MacOS style curve. Choose Logarithmic incase you want more fine adjustments in the low end, and less in the high end. In case you want to see a debug console, launch at startup or have an OSD, you can select those
in the GUI as well. If you like a more old-school deej-style config, you can find mapping.json in AppData\Roaming\RVCI.

<img width="262" height="428" alt="image" src="https://github.com/user-attachments/assets/5ccc2398-f27c-45f7-a00d-da886ec10851" />
<img width="514" height="92" alt="image" src="https://github.com/user-attachments/assets/ea747f7d-797f-4446-bf67-ec439207486a" />


## Hardware:

This project is quite customizable. For my version that I personally use you need:

- Arduino Nano
- 5x 10k Potentiometers
- A 3-Way Toggle Switch
- Whatever enclosure you can come up with (3D printed, breadboard, shoebox, whatever)

Please note that you can add as many Pots as you want, but only **1** ! 3-Way switch is currently supported!

## Installation:

1. Ensure you have a working controller connected to your PC. Use a deej tutorial or the schematic below to build one if you don't have one
2. Flash firmware.ino onto you Arduino. It can be found [here](https://github.com/TZeys/RVCI/releases)
3. Download RVCI_setup.exe from the [releases page](https://github.com/TZeys/RVCI/releases)
4. Run RVCI (It will launch minimized, check your tray icons!)
5. Right-click and select "Open Settings"
6. Configure it to your liking. Make sure you select the right COM port and Baudrate. The default is 115200, but double check!


## Important!

1. Please flash the firmware.ino found in the repo onto your Arduino Nano. The default deej firmware can cause issues.

## Schematic
**Without Mute Buttons:**

<img width="800" height="250" alt="image" src="https://github.com/user-attachments/assets/0705bf48-d2b6-495b-8a2f-7d2ab18cc0a7" />

**With Mute Buttons:**

<img width="800" height="250" alt="image" src="https://github.com/user-attachments/assets/4a55e12b-b331-46dd-9bd1-0a4c260ff85b" />




## Virtual / Output device volume control

You can also map a knob straight to a specific output device instead of a process. I mainly use this for SteelSeries Sonar, but it works just as well with Voicemeeter or anything else that exposes virtual output devices to Windows. Each virtual channel (Game, Chat, Media, Aux and so on) shows up as its own output in Windows, so you can give every channel its own knob.

To set it up, open Settings and add or edit a knob mapping, set the knob **type** to **Output Device**, then pick the output you want (e.g. "Game (SteelSeries Sonar - Gaming)") from the list. Save, and that knob now controls the master volume of that output. If a new virtual device shows up after RVCI is already running, just hit the **Update** button in the top right to refresh the list.

In `mapping.json` an output-device knob looks like this:
```json
{
  "type": "output_device",
  "process_name": "Game (SteelSeries Sonar - Gaming)",
  "inverted": false
}
```
The device gets matched by its Windows name, and it isn't case sensitive. It also does a substring match, so something like `"Game"` will resolve fine as long as it isn't ambiguous.

## Configurable Buttons

RVCI also supports physical buttons wired to the Arduino. Instead of faking a mute by hijacking a slider's signal like deej does, each button sends its own serial command, and you decide what it actually does in the GUI. A button can be:

- **Mute Knob**: mutes/unmutes a specific knob (button on = muted, off = unmuted).
- **Media**: sends a media key. Play/Pause, Next, Previous, Stop, Volume Up, Volume Down or Mute.
- **Keys**: emulates any key combo you want (e.g. `Ctrl + Shift + M`), which you record live in the GUI like a macropad.
- **None**: does nothing.

Both toggle switches and momentary buttons work. Media and Keys fire once when you press the button, while Mute just follows the physical state for as long as it's held down or toggled on.

### 1. Wire and flash (firmware side)

The button-to-pin mapping is set in the firmware before flashing. Open `firmware.ino` and edit the `buttonPins[]` array near the top:

```cpp
const int NUM_BUTTONS = 5;
const int buttonPins[NUM_BUTTONS] = {2, 3, 4, 5, 6}; // edit freely
```


### 2. Configure actions (GUI side)

1. Open Settings. Under **Button Mappings**, click **+ Add Button** once per physical button, in the same order as your `buttonPins[]`.
2. Pick an action type (None, Mute Knob, Media or Keys).
3. Fill in the second dropdown:
   - Mute Knob: pick which knob it should mute.
   - Media: pick the media key.
   - Keys: click the field and press the combo you want. It gets recorded live (e.g. `Ctrl + Alt + Del`).
4. Click **Save Changes** and you're done.

### Keys you can't map

Windows keeps a handful of key combos to itself, so the **Keys** action can't touch them no matter what:

- **Ctrl + Alt + Del** simply won't fire. It's the Secure Attention Sequence, and Windows handles it deep in the OS before any program can send it or even see it. Same story for **Ctrl + Alt + End**, which is the Remote Desktop version of the same thing.
- Anything using the **Windows key** (Win + R, Win + E, Win + L, Win + D, Win + Tab, Win + number, and so on). The recorder never sees the Win key, so you can't record those combos in the GUI. The sender itself does understand a `win` modifier, so if you really want one you can add it by hand in `mapping.json`, but Windows may still swallow it.
- **Print Screen** gets grabbed by Windows for screenshots before it ever reaches RVCI.
- The lock keys (**Caps Lock**, **Num Lock**, **Scroll Lock**), the **Menu / right-click key** and the **Fn** key don't record either.
- A lone **Esc** or **Enter** won't stick, they cancel the recording instead. Pair them with a modifier if you want something like **Ctrl + Enter**.

Everything else is fair game. Letters, numbers, F1 to F12, the arrow keys, the symbol keys and the usual **Ctrl / Shift / Alt** combos (**Ctrl + Shift + M** and friends) all record and fire just fine.

## Upcoming features and bugfixes
- Currently none


