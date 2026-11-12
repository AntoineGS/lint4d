unit W1010HidesVirtual;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  public
    procedure Invoke; virtual;
  end;

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
