unit W1009Property;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

  TChild = class(TBase)
  private
    FChildValue: Integer;
  public
    property Value: Integer read FChildValue;
  end;

implementation

end.
