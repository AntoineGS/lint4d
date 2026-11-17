unit SwitchW1010Off;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  public
    procedure Invoke; virtual;
  end;

{$WARN HIDDEN_VIRTUAL OFF}
  TChild = class(TBase)
  public
    procedure Invoke;
  end;

implementation

procedure TBase.Invoke;
begin
end;

procedure TChild.Invoke;
begin
end;

end.
