<img width="144" height="144" alt="rvci" src="https://github.com/user-attachments/assets/d20da90c-899d-4941-a34f-7a460fd544d3" /> 

# RVCI

RVCI (Rust Volume Control Interface) is a hardware-software project which can control audio devices as well as volumes in W10/11 using basic hardware

**Showcase**

[Youtube Showcase Video](https://youtu.be/VwW_25K1vdo)


This project is currently work in progress. Current repo files might or might not work properly. This version includes a fully working GUI, volume control functionality as well as audio output switcher.
RVCI was heavily inspired by Deej, however, Deej is no longer maintained and recent Windows updates heavily broke its functionality. The Idea stems from my forked version of deej "DeejXChanger", but I decided to write a new and improved lightweight version of it in Rust, and am adding the functionality I wish Deej had. 
RVCI runs on only ~2MB Ram and very little CPU.

Also, I hated writing in Go

## GUI:

The GUI lets you intuitively create, modify and delete knob mappings. Furthermore, you can easily change COM ports, Baudrate as well as between what Audio Outputs the device should switch between using a physical switch connected to the Arduino. A "Launch at Startup" option was also included,
as deej never seemed to get it right somehow.

<img width="284" height="487" alt="image" src="https://github.com/user-attachments/assets/e3cd1118-3f1b-4c19-9620-48b36cd845d8" />


## Hardware:

This project is quite customizable. For my version that I personally use you need:

- Arduino Nano
- 5x 10k Potentiometers
- A 3-Way Toggle Switch
- Whatever enclosure you can come up with (3D printed, breadboard, shoebox, whatever)

Please note that you can add as many Pots as you want, but only **1** ! 3-Way switch is currently supported!

## Important!

1. Please flash the firmware.ino found in the repo onto your Arduino Nano, otherwise the companion app won't work

## Schematic
**Without Mute Buttons:**

<img width="800" height="250" alt="image" src="https://github.com/user-attachments/assets/0705bf48-d2b6-495b-8a2f-7d2ab18cc0a7" />

**With Mute Buttons:**

<img width="800" height="250" alt="image" src="https://github.com/user-attachments/assets/4a55e12b-b331-46dd-9bd1-0a4c260ff85b" />

note: these mute buttons function without any changes to the arduino firmware. No firmware upgrades needed



## Upcoming features and bugfixes
- None currently


