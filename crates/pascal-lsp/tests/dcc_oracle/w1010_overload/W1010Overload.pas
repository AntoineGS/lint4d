unit W1010Overload;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  public
    procedure Invoke; overload; virtual;
  end;

  TChild = class(TBase)
  public
    procedure Invoke(Value: Integer); overload;
  end;

implementation

procedure TBase.Invoke;
begin
end;

procedure TChild.Invoke(Value: Integer);
begin
end;

end.
