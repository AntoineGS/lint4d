unit InterfaceDeclaration;

interface

type
  IMyInterface = interface
    ['{A1B2C3D4-E5F6-7890-ABCD-EF1234567890}']
    procedure DoSomething;
    function GetValue: Integer;
  end;

  TMyImplementation = class(TInterfacedObject, IMyInterface)
  public
    procedure DoSomething;
    function GetValue: Integer;
  end;

implementation

procedure TMyImplementation.DoSomething;
begin
  WriteLn('done');
end;

function TMyImplementation.GetValue: Integer;
begin
  Result := 42;
end;

end.
