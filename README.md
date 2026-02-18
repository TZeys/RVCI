<img width="144" height="144" alt="rvci" src="https://github.com/user-attachments/assets/d20da90c-899d-4941-a34f-7a460fd544d3" /> 

# RVCI

RVCI (Rust Volume Control Interface) is a hardware-software project which can control individual app volumes in W10/11 using basic hardware

**Showcase**

[Youtube Showcase Video](https://youtu.be/VwW_25K1vdo)


This project is currently work in progress. Current repo files might or might not work properly. This version includes a fully working GUI, volume control functionality as well as audio output switcher.
RVCI was heavily inspired by Deej, however, Deej is no longer maintained and recent Windows updates heavily broke its functionality. The Idea stems from my forked version of deej "DeejXChanger", but I decided to write a new and improved lightweight version of it in Rust, and am adding the functionality I wish Deej had. 
RVCI runs on only ~2MB Ram and very little CPU.

Also, I hated writing in Go

## GUI:

The GUI lets you intuitively create, modify and delete knob mappings. Furthermore, you can easily change COM ports, Baudrate as well as between what Audio Outputs the device should switch between using a physical switch connected to the Arduino. A "Launch at Startup" option was also included,
as deej never seemed to get it right somehow.

<img width="281" height="489" alt="image" src="https://github.com/user-attachments/assets/8375a57a-db30-4431-aba9-d4a8df22741a" />

## Hardware:

This project is quite customizable. For my version that I personally use you need:

- Arduino Nano
- 5x 10k Potentiometers
- A 3-Way Toggle Switch
- Whatever enclosure you can come up with (3D printed, breadboard, shoebox, whatever)

Please note that you can add as many Pots as you want, but only **1** ! 3-Way switch is currently supported!

## Important!
1. This software requires SoundVolumeView! You can find it [here](https://www.nirsoft.net/utils/sound_volume_view.html)

After installation of both SoundVolumeView and RVCI, please set the directory path
to the SoundVolumeView.exe in the mapping.json file!

2. Please flash the firmware.ino found in the repo onto your Arduino Nano, otherwise the companion app won't work

## Schematic

<img width="800" height="250" alt="image" src="https://github.com/user-attachments/assets/0705bf48-d2b6-495b-8a2f-7d2ab18cc0a7" />



## Upcoming features and bugfixes

- Fixing a bug causing duplication of knob mappings when deleting and adding a mapping
- Implementing an "invert" toggle per knob
- THT PCB for simple setups



