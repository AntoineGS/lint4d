unit SignatureMismatch;
interface
type
  TObject = class
  end;
  TBase = class(TObject)
    procedure Run(Value: Integer); virtual;
  end;
  TChild = class(TBase)
    procedure Run(Value: string); override;
  end;
implementation
procedure TBase.Run(Value: Integer);
begin
end;
procedure TChild.Run(Value: string);
begin
end;
end.
