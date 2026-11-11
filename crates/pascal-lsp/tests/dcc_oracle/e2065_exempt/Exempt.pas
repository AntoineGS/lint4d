unit Exempt;

interface

uses
  System.Classes;

type
  IFoo = interface
    procedure Run;
  end;

  TAbstract = class
    procedure AbstractRun; virtual; abstract;
  end;

  TResolved = class(TInterfacedObject, IFoo)
    procedure Implemented;
    procedure IFoo.Run = Implemented;
  end;

procedure Beep(Frequency, Duration: Cardinal); stdcall;
  external 'kernel32.dll' name 'Beep';

implementation

procedure TResolved.Implemented;
begin
end;

end.
