#include <Arduino.h>
#include <Keyboard.h>

//#ifdef __cplusplus
//extern "C" {
//#endif
//void setup();
//void loop();
//#ifdef __cplusplus
//}
//#endif

void updateSliderValues();
void sendSliderValues();

const int NUM_SLIDERS = 5;
const int analogInputs[NUM_SLIDERS] = {A1, A2, A3, A4, A5};
const int switchPin2 = 7; // Pin for switch 2
const int switchPin3 = 6; // Pin for switch 3

int analogSliderValues[NUM_SLIDERS];
int prevSwitchState2 = LOW;
int prevSwitchState3 = LOW;

void setup() {
  pinMode(switchPin2, INPUT_PULLUP);
  pinMode(switchPin3, INPUT_PULLUP);
  for (int i = 0; i < NUM_SLIDERS; i++) {
    pinMode(analogInputs[i], INPUT);
  }

  Serial.begin(9600);
}

void loop() {
  updateSliderValues();
  delay(20);
  sendSliderValues(); // Actually send data (all the time)
  delay(100);
  int switchState2 = digitalRead(switchPin2);
  int switchState3 = digitalRead(switchPin3);
  // printSliderValues(); // For debug
 if (switchState2 != prevSwitchState2) {
    if (switchState2 == HIGH) {
      Serial.println("WORKS 2");
    }
    prevSwitchState2 = switchState2;
    delay(50); // Debounce delay
  }

  // Check for switch 3 state change
  if (switchState3 != prevSwitchState3) {
    if (switchState3 == HIGH) {
      Serial.println("WORKS 1");
    }
    prevSwitchState3 = switchState3;
    delay(50); // Debounce delay
  }
  delay(20);
}

void updateSliderValues() {
  for (int i = 0; i < NUM_SLIDERS; i++) {
     analogSliderValues[i] = analogRead(analogInputs[i]);
  }
}

void sendSliderValues() {
  String builtString = String("");

  for (int i = 0; i < NUM_SLIDERS; i++) {
    builtString += String((int)analogSliderValues[i]);

    if (i < NUM_SLIDERS - 1) {
      builtString += String("|");
    }
  }
  
 Serial.println(builtString);
}

void printSliderValues() {
  for (int i = 0; i < NUM_SLIDERS; i++) {
    String printedString = String("Slider #") + String(i + 1) + String(": ") + String(analogSliderValues[i]) + String(" mV");
    Serial.write(printedString.c_str());

    if (i < NUM_SLIDERS - 1) {
      Serial.write(" | ");
    } else {
      Serial.write("\n");
    }
  }
}
