<img width="144" height="144" alt="rvci" src="https://github.com/user-attachments/assets/d20da90c-899d-4941-a34f-7a460fd544d3" /> 

# RVCI

RVCI (Rust Volume Control Interface) is a hardware-software project which can control audio devices as well as volumes in W10/11 using basic hardware

**Showcase**

[Youtube Showcase Video](https://youtu.be/tl9Kyg_MPLU)


This project is currently work in progress. Current repo files might or might not work properly. This version includes a fully working GUI for configuration, OSD for seeing volume changes, volume control functionality for output and microphones, volume curves, as well as an audio output switcher.
RVCI was heavily inspired by Deej, however, Deej is no longer maintained and recent Windows updates heavily broke its functionality. The Idea stems from my forked version of deej "DeejXChanger", but I decided to write a new and improved lightweight version of it in Rust, and am adding the functionality I wish Deej had. 
RVCI runs on only ~2MB Ram and very little CPU.

Also, I hated writing in Go

## GUI:

The GUI lets you intuitively create, modify and delete knob mappings. Furthermore, you can easily change COM ports, Baudrate as well as between what Audio Outputs the device should switch between using a physical switch connected to the Arduino. 
Furthermore, you can choose between a linear volume curve, or a logarithmic MacOS style curve. Choose Logarithmic incase you want more fine adjustments in the low end, and less in the high end. In case you want to see a debug console, launch at startup or have an OSD, you can select those
in the GUI as well. If you like a more old-school deej-style config, you can find mapping.json in AppData\Roaming\RVCI.

<img width="262" height="428" alt="image" src="https://github.com/user-attachments/assets/5ccc2398-f27c-45f7-a00d-da886ec10851" />
<img width="257" height="46" alt="image" src="https://github.com/user-attachments/assets/ea747f7d-797f-4446-bf67-ec439207486a" />


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

note: these mute buttons function without any changes to the arduino firmware. No firmware upgrades needed



## Upcoming features and bugfixes
- Currently none


