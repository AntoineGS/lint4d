unit GoodNaming;

interface

type
  IDoable = interface
    procedure DoSomething;
  end;

  TMyClass = class(TInterfacedObject, IDoable)
  public
    procedure DoSomething;
  end;

  TMyRecord = record
    X: Integer;
  end;

implementation

procedure TMyClass.DoSomething;
begin
end;

end.
