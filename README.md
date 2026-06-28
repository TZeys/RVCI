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

**Incase you open a program which you want to map AFTER launching RVCI, click the update button in the top right. The application should now be selectable in the knob mappings section!**

## Important!

1. Please flash the firmware.ino found in the repo onto your Arduino Nano. The default deej firmware can cause issues.

## Schematic
**Without Mute Buttons:**

<img width="800" height="250" alt="image" src="https://github.com/user-attachments/assets/0705bf48-d2b6-495b-8a2f-7d2ab18cc0a7" />

**With Mute Buttons:**

<img width="800" height="250" alt="image" src="https://github.com/user-attachments/assets/4a55e12b-b331-46dd-9bd1-0a4c260ff85b" />




## Virtual / Output device volume control

RVCI can map a knob directly to the volume of a specific **playback (output) device**, not just a process. This is ideal for software like **SteelSeries Sonar**, **Voicemeeter** or any tool that exposes **virtual audio output devices** to Windows. Each virtual channel (e.g. Game, Chat, Media, Aux) appears as its own output endpoint, so you can tie a separate knob to each one.
**How to use it:**
1. Open Settings and add (or edit) a knob mapping.
2. Set the knob **type** dropdown to **Output Device**.
3. Pick the target output device (e.g. "Game (SteelSeries Sonar - Gaming)") from the device list.
4. Save changes. Turning that knob now controls the master volume of that output endpoint.

If a new virtual device appears after RVCI is already running, click the **Update** button in the top right to refresh the device list.

In `mapping.json`, an output-device knob looks like this:
```json
{
  "type": "output_device",
  "process_name": "Game (SteelSeries Sonar - Gaming)",
  "inverted": false
}
```
The device is matched by its Windows friendly name (case-insensitive, with substring fallback), so a partial name such as `"Game"` will also resolve as long as it is unambiguous.

## Configurable Buttons

RVCI now supports **physical buttons** wired to the Arduino. Instead of overwriting a slider's signal to fake a mute, each button now sends a dedicated serial command to RVCI, and **you decide in the GUI what each button does**. A single button can:

- **Mute Knob** — mute/unmute a specific knob mapping (state-driven: button ON = muted, OFF = unmuted).
- **Media** — send a media key: Play/Pause, Next Track, Prev Track, Stop, Volume Up, Volume Down, Volume Mute.
- **Keys** — emulate any key combination (e.g. `Ctrl + Shift + M`), recorded live in the GUI like a macropad.
- **None** — disabled.

Both **toggle** switches and **momentary** buttons work. Media and Keys actions fire once on the OFF→ON press; Mute mirrors the physical state for as long as the button is held/toggled.

### 1. Wire and flash (firmware side)

The button-to-pin mapping is set **in the firmware before flashing**. Open `firmware.ino` and edit the `buttonPins[]` array near the top:

```cpp
const int NUM_BUTTONS = 5;
const int buttonPins[NUM_BUTTONS] = {2, 3, 4, 5, 6}; // edit freely
```


### 2. Configure actions (GUI side)

1. Open Settings. Under **Button Mappings**, click **+ Add Button** once per physical button (in the same order as `buttonPins[]`).
2. Pick an **action type** (None / Mute Knob / Media / Keys).
3. Fill in the secondary control:
   - *Mute Knob* → choose which knob to mute.
   - *Media* → choose the media key.
   - *Keys* → click the field and **press the desired key combination** — it is recorded live (e.g. `Ctrl + Alt + Del`).
4. Click **Save Changes** 

## Upcoming features and bugfixes
- Currently none


