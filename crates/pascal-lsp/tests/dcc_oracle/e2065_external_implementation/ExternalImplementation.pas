unit ExternalImplementation;

interface

procedure Beep(Frequency, Duration: Cardinal); stdcall;

implementation

procedure Beep(Frequency, Duration: Cardinal); stdcall;
  external 'kernel32.dll' name 'Beep';

end.
