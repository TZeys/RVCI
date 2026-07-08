#include <Arduino.h>

void updateSliderValues();
void sendSliderValues();
void updateButtons();

const int NUM_SLIDERS = 5;
const int analogInputs[NUM_SLIDERS] = {A1, A2, A3, A4, A5};

const int NUM_BUTTONS = 5;
const int buttonPins[NUM_BUTTONS] = {2, 3, 4, 5, 6};

const int switchPin1 = 8;
const int switchPin2 = 7;

int analogSliderValues[NUM_SLIDERS];
float smoothedValues[NUM_SLIDERS];

const float SMOOTHING_FACTOR = 0.90;
const int NOISE_GATE = 12;
const int MIN_VAL = 15;
const int MAX_VAL = 1010;

int prevSwitchState1 = HIGH;
int prevSwitchState2 = HIGH;
bool buttonState[NUM_BUTTONS];
bool lastReading[NUM_BUTTONS];
unsigned long lastDebounceTime[NUM_BUTTONS];
const unsigned long DEBOUNCE_MS = 25;

void setup() {
  pinMode(switchPin1, INPUT_PULLUP);
  pinMode(switchPin2, INPUT_PULLUP);

  for (int i = 0; i < NUM_SLIDERS; i++) {
    pinMode(analogInputs[i], INPUT);
    int startRead = analogRead(analogInputs[i]);
    smoothedValues[i] = startRead;
    analogSliderValues[i] = map(startRead, MIN_VAL, MAX_VAL, 0, 1023);
  }

  for (int i = 0; i < NUM_BUTTONS; i++) {
    pinMode(buttonPins[i], INPUT_PULLUP);
    bool pressed = (digitalRead(buttonPins[i]) == LOW);
    buttonState[i] = pressed;
    lastReading[i] = pressed;
    lastDebounceTime[i] = 0;
  }

  Serial.begin(115200);

  for (int i = 0; i < NUM_BUTTONS; i++) {
    Serial.print("BTN ");
    Serial.print(i);
    Serial.print(" ");
    Serial.println(buttonState[i] ? 1 : 0);
  }
}

void loop() {
  updateSliderValues();
  delay(10);
  sendSliderValues();

  updateButtons();

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

    smoothedValues[i] = (smoothedValues[i] * (1.0 - SMOOTHING_FACTOR)) + (raw * SMOOTHING_FACTOR);

    int currentMapped = constrain(map((int)smoothedValues[i], MIN_VAL, MAX_VAL, 0, 1023), 0, 1023);

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

void updateButtons() {
  for (int i = 0; i < NUM_BUTTONS; i++) {
    bool reading = (digitalRead(buttonPins[i]) == LOW);

    if (reading != lastReading[i]) {
      lastDebounceTime[i] = millis();
      lastReading[i] = reading;
    }

    if ((millis() - lastDebounceTime[i]) > DEBOUNCE_MS) {
      if (reading != buttonState[i]) {
        buttonState[i] = reading;
        Serial.print("BTN ");
        Serial.print(i);
        Serial.print(" ");
        Serial.println(buttonState[i] ? 1 : 0);
      }
    }
  }
}
