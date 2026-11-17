unit E2003Attributes;
interface
implementation
type
  TObject = class end;
  TAttributed = class
  private
    [Weak] FRef: TObject;
  public
    [MissingAttr] procedure P;
  end;
procedure TAttributed.P;
begin
end;
end.
