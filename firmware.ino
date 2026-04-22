#include <Arduino.h>

void updateSliderValues();
void sendSliderValues();

const int NUM_SLIDERS = 5;
const int analogInputs[NUM_SLIDERS] = {A1, A2, A3, A4, A5};
const int switchPin1 = 6; 
const int switchPin2 = 7; 

int analogSliderValues[NUM_SLIDERS];
float smoothedValues[NUM_SLIDERS];

// ==========================================
// CONFIGURABLE FILTERING VARIABLES
// ==========================================
// 0.01 to 1.0. Lower = smoother/slower. Higher = snappier/noisier.
const float SMOOTHING_FACTOR = 0.15; 

// Increase incase of jittery output
const int NOISE_GATE = 12; 

// Deadzone Compensation: adjust these if you can't reach 0% or 100% volume.
const int MIN_VAL = 15;    
const int MAX_VAL = 1010;  
// ==========================================

int prevSwitchState1 = HIGH;
int prevSwitchState2 = HIGH;

void setup() {
  pinMode(switchPin1, INPUT_PULLUP);
  pinMode(switchPin2, INPUT_PULLUP);
  
  for (int i = 0; i < NUM_SLIDERS; i++) {
    pinMode(analogInputs[i], INPUT);
    int startRead = analogRead(analogInputs[i]);
    smoothedValues[i] = startRead;
    analogSliderValues[i] = map(startRead, MIN_VAL, MAX_VAL, 0, 1023);
  }

  Serial.begin(115200); 
}

void loop() {
  updateSliderValues();
  sendSliderValues();

  // Switch Logic 
  int s1 = digitalRead(switchPin1);
  int s2 = digitalRead(switchPin2);
  if (s1 == LOW && prevSwitchState1 == HIGH) { Serial.println("WORKS 1"); delay(50); }
  prevSwitchState1 = s1;
  if (s2 == LOW && prevSwitchState2 == HIGH) { Serial.println("WORKS 2"); delay(50); }
  prevSwitchState2 = s2;

  delay(15); 
}

void updateSliderValues() {
  for (int i = 0; i < NUM_SLIDERS; i++) {
    int raw = analogRead(analogInputs[i]);
    
    // exponential moving average smoothing
    smoothedValues[i] = (smoothedValues[i] * (1.0 - SMOOTHING_FACTOR)) + (raw * SMOOTHING_FACTOR);

    // value mapping
    int currentMapped = constrain(map((int)smoothedValues[i], MIN_VAL, MAX_VAL, 0, 1023), 0, 1023);

    //noise gate
    if (abs(currentMapped - analogSliderValues[i]) > NOISE_GATE) {
      analogSliderValues[i] = currentMapped;
    }
    
    if (currentMapped < 8) analogSliderValues[i] = 0;
    if (currentMapped > 1015) analogSliderValues[i] = 1023;
  }
}

void sendSliderValues() {
  String builtString = "";
  for (int i = 0; i < NUM_SLIDERS; i++) {
    builtString += String(analogSliderValues[i]);
    if (i < NUM_SLIDERS - 1) {
      builtString += "|";
    }
  }
  Serial.println(builtString);
}