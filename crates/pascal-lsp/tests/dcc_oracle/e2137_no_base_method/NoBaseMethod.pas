unit NoBaseMethod;
interface
type
  TObject = class
  end;
  TBase = class(TObject)
  end;
  TChild = class(TBase)
    procedure Missing; override;
  end;
implementation
procedure TChild.Missing;
begin
end;
end.
