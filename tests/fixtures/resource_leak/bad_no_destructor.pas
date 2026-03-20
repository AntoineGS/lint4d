unit BadNoDestructor;

interface

type
  TNoDestructor = class
  private
    FChild: TObject;
  public
    constructor Create;
  end;

implementation

constructor TNoDestructor.Create;
begin
  inherited Create;
  FChild := TObject.Create;
end;

end.
