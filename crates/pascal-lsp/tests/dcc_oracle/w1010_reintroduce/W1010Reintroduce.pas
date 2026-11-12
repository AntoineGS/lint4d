unit W1010Reintroduce;

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
    procedure Invoke; reintroduce;
  end;

implementation

procedure TBase.Invoke;
begin
end;

procedure TChild.Invoke;
begin
end;

end.
