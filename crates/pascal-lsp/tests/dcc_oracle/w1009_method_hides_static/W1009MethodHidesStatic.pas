unit W1009MethodHidesStatic;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  public
    procedure Store;
  end;

  TChild = class(TBase)
  public
    procedure Store;
  end;

implementation

procedure TBase.Store;
begin
end;

procedure TChild.Store;
begin
end;

end.
