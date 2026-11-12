unit OverrideValid;
interface
type
  TObject = class
  end;
  TBase = class(TObject)
    procedure Run; virtual;
  end;
  TChild = class(TBase)
    procedure Run; override;
  end;
implementation
procedure TBase.Run;
begin
end;
procedure TChild.Run;
begin
end;
end.
